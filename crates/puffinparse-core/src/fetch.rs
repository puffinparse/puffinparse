//! The one place PuffinParse downloads a caller-supplied document URL itself.
//!
//! Some providers cannot take a URL (Gemini, Textract, the vision LLMs, the local engines, ...),
//! so a [`DocumentInput::Url`](crate::types::DocumentInput) is fetched in this process and the
//! bytes are uploaded instead. A URL chosen by someone else (a gateway caller, a user of an app
//! built on the SDK) would then make this process issue requests on its network: server-side
//! request forgery against cloud metadata endpoints, admin ports on localhost, or hosts on the
//! private network. Every such download goes through [`fetch_document`], which:
//!
//! - accepts `http` and `https` only;
//! - resolves the host itself and refuses loopback, private (RFC 1918), link-local (including
//!   `169.254.169.254`), CGNAT `100.64.0.0/10`, unique-local `fc00::/7`, unspecified, multicast,
//!   broadcast, documentation/benchmarking/reserved ranges, and IPv6 forms that embed one of those
//!   (IPv4-mapped, IPv4-compatible, NAT64, 6to4) — then connects only to an address it vetted, so a
//!   second DNS answer cannot swap in a private address (DNS rebinding). Host header and TLS SNI
//!   stay the URL's host name;
//! - ignores `HTTP(S)_PROXY` (a proxy would resolve the name instead of us);
//! - follows at most [`MAX_REDIRECTS`] redirects by hand, re-validating every hop;
//! - stops reading at [`FetchPolicy::max_bytes`] (default 50 MiB; decompressed size) and within the
//!   request's deadline;
//! - never puts the response body into an error message.
//!
//! Trusted setups that do want private addresses (a document server on the LAN, tests) opt in
//! process-wide with `PUFFINPARSE_ALLOW_PRIVATE_URLS=1`, or, for an embedding application such as
//! the gateway, [`set_process_policy`]. The limit is `PUFFINPARSE_MAX_DOWNLOAD_MB`. There is
//! deliberately no per-request switch: whoever writes the request must not be able to lift it.
//!
//! Which providers download in-process is [`fetches_url_in_process`], the single classification
//! the gateway uses for `server.fetch_document_urls`.

use crate::error::{Error, ErrorKind, Result};
use crate::http::{self, Deadline, Retry};
use crate::types::Mode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// Redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;
/// Default download cap, in MiB.
pub const DEFAULT_MAX_DOWNLOAD_MB: u64 = 50;
/// `1` / `true` / `yes` lets downloads reach private and loopback addresses (trusted setups only).
pub const ENV_ALLOW_PRIVATE: &str = "PUFFINPARSE_ALLOW_PRIVATE_URLS";
/// Download cap in MiB (default [`DEFAULT_MAX_DOWNLOAD_MB`]).
pub const ENV_MAX_DOWNLOAD_MB: &str = "PUFFINPARSE_MAX_DOWNLOAD_MB";

/// Providers that download a URL input in this process (always, in every mode) instead of handing
/// the URL to the provider. The local engines are listed even where their server could fetch a
/// URL itself (docling-serve, PaddleOCR serving): that server sits on the operator's network, so
/// letting it fetch an arbitrary URL is the same server-side request as fetching it here.
const FETCHES_IN_PROCESS: &[&str] = &[
    "anthropic",
    "docling",
    "gemini",
    "google_documentai",
    "openai",
    "paddleocr",
    "tesseract",
    "textract",
    "unstructured",
    "upstage",
    "vllm",
];

/// Whether `provider` downloads a `document_url` in this process for `mode` (through
/// [`fetch_document`]), rather than passing the URL to the provider's own API.
///
/// Everything else passes the URL through: Reducto, Extend, LlamaParse parse, Mistral, Azure,
/// Datalab, Mathpix, Landing AI and OpenDocRouter fetch it on their side. LlamaParse `extract`
/// (LlamaExtract takes file ids only) downloads and re-uploads.
pub fn fetches_url_in_process(provider: &str, mode: Mode) -> bool {
    let p = provider.to_ascii_lowercase();
    FETCHES_IN_PROCESS.contains(&p.as_str()) || (p == "llamaparse" && mode == Mode::Extract)
}

/// What downloads may do. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchPolicy {
    /// Allow loopback / private / link-local / ... destinations. Off by default.
    pub allow_private: bool,
    /// Largest body read, in bytes.
    pub max_bytes: u64,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self { allow_private: false, max_bytes: DEFAULT_MAX_DOWNLOAD_MB * 1024 * 1024 }
    }
}

impl FetchPolicy {
    /// The policy from `PUFFINPARSE_ALLOW_PRIVATE_URLS` and `PUFFINPARSE_MAX_DOWNLOAD_MB`.
    pub fn from_env() -> Self {
        let allow_private = std::env::var(ENV_ALLOW_PRIVATE)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let max_bytes = std::env::var(ENV_MAX_DOWNLOAD_MB)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .unwrap_or(DEFAULT_MAX_DOWNLOAD_MB)
            .saturating_mul(1024 * 1024);
        Self { allow_private, max_bytes }
    }
}

fn process_policy() -> &'static RwLock<Option<FetchPolicy>> {
    static POLICY: OnceLock<RwLock<Option<FetchPolicy>>> = OnceLock::new();
    POLICY.get_or_init(|| RwLock::new(None))
}

/// Fix the download policy for this process, overriding the environment variables. Meant for an
/// application that owns its configuration (the gateway sets it from `puffinparse.toml`).
pub fn set_process_policy(policy: FetchPolicy) {
    *process_policy().write().unwrap_or_else(|e| e.into_inner()) = Some(policy);
}

/// The policy in force: [`set_process_policy`]'s if set, else [`FetchPolicy::from_env`].
pub fn current_policy() -> FetchPolicy {
    process_policy().read().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(FetchPolicy::from_env)
}

/// A downloaded document.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub data: bytes::Bytes,
    /// The response `Content-Type` essence, lower-cased (`None` when absent).
    pub content_type: Option<String>,
}

impl Fetched {
    /// The response's content type unless it is absent or a generic octet-stream, else `fallback`
    /// (the URL's extension is more reliable than `application/octet-stream`).
    pub fn mime_or(&self, fallback: String) -> String {
        match &self.content_type {
            Some(ct) if !ct.is_empty() && ct != "application/octet-stream" && ct != "binary/octet-stream" => ct.clone(),
            _ => fallback,
        }
    }
}

/// Download `url` under the process policy, with retries on transient failures.
pub async fn fetch_document(provider: &str, url: &str, deadline: &Deadline, retry: Retry) -> Result<Fetched> {
    fetch_with(&current_policy(), provider, url, deadline, retry).await
}

/// [`fetch_document`] with an explicit policy.
pub async fn fetch_with(
    policy: &FetchPolicy,
    provider: &str,
    url: &str,
    deadline: &Deadline,
    retry: Retry,
) -> Result<Fetched> {
    let parsed = url::Url::parse(url).map_err(|e| Error::input(format!("invalid document URL: {e}")))?;
    check_url(policy, &parsed).map_err(|e| e.with_provider(provider))?;
    tracing::debug!(provider, host = parsed.host_str().unwrap_or(""), "downloading document URL");
    let fetched = http::with_retry(provider, retry, deadline, || fetch_once(policy, parsed.clone(), deadline))
        .await
        .map_err(|e| e.with_provider(provider))?;
    if fetched.data.is_empty() {
        return Err(Error::input("the document URL returned an empty body").with_provider(provider));
    }
    Ok(fetched)
}

/// One attempt: the request plus its redirects.
async fn fetch_once(policy: &FetchPolicy, mut url: url::Url, deadline: &Deadline) -> Result<Fetched> {
    let client = client(policy.allow_private);
    for hop in 0..=MAX_REDIRECTS {
        let send = client.get(url.clone()).timeout(deadline.request_timeout()).send();
        let resp = match tokio::time::timeout(deadline.remaining(), send).await {
            Ok(r) => r.map_err(map_send_error)?,
            Err(_) => return Err(Error::timeout("deadline exceeded while downloading the document URL")),
        };
        let status = resp.status();
        if status.is_redirection() {
            let location = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok());
            let Some(location) = location else {
                return Err(Error::input(format!(
                    "the document URL answered HTTP {} without a Location",
                    status.as_u16()
                )));
            };
            if hop == MAX_REDIRECTS {
                return Err(Error::input(format!("the document URL redirected more than {MAX_REDIRECTS} times")));
            }
            let next = url.join(location).map_err(|e| Error::input(format!("invalid redirect target: {e}")))?;
            check_url(policy, &next)?;
            url = next;
            continue;
        }
        if !status.is_success() {
            // The status only: an error body can be anything the remote host chose to send.
            let msg = format!("could not download the document URL: HTTP {}", status.as_u16());
            let kind = match status.as_u16() {
                429 | 500..=599 => ErrorKind::Network,
                _ => ErrorKind::Input,
            };
            return Err(Error::new(kind, msg).with_status(status.as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > policy.max_bytes) {
            return Err(too_large(policy));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
        let data = read_capped(resp, policy.max_bytes, deadline).await?;
        return Ok(Fetched { data, content_type });
    }
    unreachable!("the loop returns on the last hop")
}

async fn read_capped(mut resp: reqwest::Response, max: u64, deadline: &Deadline) -> Result<bytes::Bytes> {
    let mut buf = bytes::BytesMut::new();
    loop {
        let chunk = match tokio::time::timeout(deadline.remaining(), resp.chunk()).await {
            Ok(c) => c.map_err(map_send_error)?,
            Err(_) => return Err(Error::timeout("deadline exceeded while downloading the document URL")),
        };
        let Some(chunk) = chunk else { break };
        if buf.len() as u64 + chunk.len() as u64 > max {
            return Err(too_large(&FetchPolicy { allow_private: false, max_bytes: max }));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

fn too_large(policy: &FetchPolicy) -> Error {
    Error::input(format!(
        "the document URL is larger than the {} MiB download limit ({ENV_MAX_DOWNLOAD_MB})",
        policy.max_bytes / (1024 * 1024)
    ))
}

/// Scheme and literal-address checks (names are checked when they are resolved).
fn check_url(policy: &FetchPolicy, url: &url::Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::input(format!("document URLs must be http or https, not '{}'", url.scheme())));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::input("document URLs must not carry credentials (user:password@)"));
    }
    let ip = match url.host() {
        None => return Err(Error::input("the document URL has no host")),
        Some(url::Host::Ipv4(v4)) => IpAddr::V4(v4),
        Some(url::Host::Ipv6(v6)) => IpAddr::V6(v6),
        Some(url::Host::Domain(_)) => return Ok(()),
    };
    if policy.allow_private || is_public(ip) {
        Ok(())
    } else {
        Err(blocked_error())
    }
}

fn blocked_error() -> Error {
    Error::input(format!(
        "the document URL points to a private, loopback, link-local or otherwise non-public address, which \
         PuffinParse does not download from (set {ENV_ALLOW_PRIVATE}=1 in a trusted setup to allow it)"
    ))
}

/// Raised by the resolver; found again in the reqwest error chain by [`map_send_error`].
#[derive(Debug)]
struct Blocked;

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("host resolves only to non-public addresses")
    }
}

impl std::error::Error for Blocked {}

fn map_send_error(e: reqwest::Error) -> Error {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(s) = source {
        if s.is::<Blocked>() {
            return blocked_error();
        }
        source = s.source();
    }
    if e.is_timeout() {
        return Error::timeout("timed out downloading the document URL");
    }
    // `without_url`: the URL may carry a signature or token in its query string.
    Error::network(format!("could not download the document URL: {}", e.without_url()))
}

/// Resolves with the system resolver and keeps public addresses only.
#[derive(Debug)]
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| is_public(a.ip())).collect();
            if public.is_empty() {
                return Err(Box::new(Blocked) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Two clients: the default one (public addresses only, no proxy) and the opt-in trusted one.
/// Both follow no redirects on their own.
fn client(allow_private: bool) -> &'static reqwest::Client {
    static SAFE: OnceLock<reqwest::Client> = OnceLock::new();
    static TRUSTED: OnceLock<reqwest::Client> = OnceLock::new();
    let build = |safe: bool| {
        let mut b = reqwest::Client::builder()
            .user_agent(concat!("puffinparse/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none());
        if safe {
            b = b.no_proxy().dns_resolver(Arc::new(PublicOnly));
        }
        b.build().expect("reqwest client builds")
    };
    if allow_private {
        TRUSTED.get_or_init(|| build(false))
    } else {
        SAFE.get_or_init(|| build(true))
    }
}

/// `true` for a globally routable unicast address; `false` for everything a document download
/// must not reach (see the module docs).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = a == 0                                  // "this network", incl. 0.0.0.0
        || a == 10                                        // RFC 1918
        || a == 127                                       // loopback
        || (a == 100 && (64..128).contains(&b))           // CGNAT 100.64.0.0/10
        || (a == 169 && b == 254)                         // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b))            // RFC 1918
        || (a == 192 && b == 0 && c == 0)                 // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)                 // TEST-NET-1
        || (a == 192 && b == 88 && c == 99)               // 6to4 relay anycast
        || (a == 192 && b == 168)                         // RFC 1918
        || (a == 198 && (b == 18 || b == 19))             // benchmarking
        || (a == 198 && b == 51 && c == 100)              // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)               // TEST-NET-3
        || a >= 224; // multicast 224/4, reserved 240/4, broadcast
    !blocked
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // IPv4-mapped ::ffff:a.b.c.d
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    // Unspecified, loopback and the deprecated IPv4-compatible ::a.b.c.d (all start with 96 zero bits).
    if s[..6] == [0, 0, 0, 0, 0, 0] {
        return false;
    }
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    match s[0] {
        // NAT64 64:ff9b::/96 and the local-use 64:ff9b:1::/48 embed an IPv4 address.
        0x0064 if s[1] == 0xff9b => s[2] == 0 && is_public_v4(embedded(s[6], s[7])),
        // Discard-only 100::/64.
        0x0100 if s[1..4] == [0, 0, 0] => false,
        // 2001::/23 IETF assignments (Teredo 2001::/32, ORCHID, ...) and 2001:db8::/32 documentation.
        0x2001 if s[1] < 0x0200 || s[1] == 0x0db8 => false,
        // 6to4 2002::/16 embeds an IPv4 address in bits 16..48.
        0x2002 => is_public_v4(embedded(s[1], s[2])),
        // Unique-local fc00::/7, link-local fe80::/10, site-local fec0::/10, multicast ff00::/8.
        x if (x & 0xfe00) == 0xfc00 || (x & 0xffc0) == 0xfe80 || (x & 0xffc0) == 0xfec0 || (x & 0xff00) == 0xff00 => {
            false
        }
        // Only 2000::/3 is allocated global unicast.
        x => (x & 0xe000) == 0x2000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn classifies_addresses() {
        for bad in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "198.18.0.1",
            "192.0.2.10",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.1.2.3",
            "::127.0.0.1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "64:ff9b::a9fe:a9fe", // NAT64 of 169.254.169.254
            "2002:7f00:1::",      // 6to4 of 127.0.0.1
            "2001:db8::1",
            "2001::1", // Teredo
        ] {
            assert!(!is_public(bad.parse().unwrap()), "{bad} should be refused");
        }
        for good in
            ["8.8.8.8", "1.1.1.1", "172.32.0.1", "100.128.0.1", "2606:4700::1111", "::ffff:8.8.8.8", "64:ff9b::808:808"]
        {
            assert!(is_public(good.parse().unwrap()), "{good} should be allowed");
        }
    }

    #[test]
    fn classification_of_in_process_downloads() {
        // In-process: always (all modes).
        for p in FETCHES_IN_PROCESS {
            for m in [Mode::Parse, Mode::Ocr, Mode::Extract] {
                assert!(fetches_url_in_process(p, m), "{p}");
            }
        }
        // Passed through to the provider.
        for p in ["reducto", "extend", "mistral", "azure", "datalab", "mathpix", "landingai", "opendocrouter"] {
            for m in [Mode::Parse, Mode::Ocr, Mode::Extract] {
                assert!(!fetches_url_in_process(p, m), "{p}");
            }
        }
        assert!(!fetches_url_in_process("llamaparse", Mode::Parse));
        assert!(!fetches_url_in_process("llamaparse", Mode::Ocr));
        assert!(fetches_url_in_process("llamaparse", Mode::Extract));
        // Every registered provider is classified on purpose: a new one must be added to one side.
        let known_passthrough =
            ["reducto", "extend", "llamaparse", "mistral", "azure", "datalab", "mathpix", "landingai", "opendocrouter"];
        for p in crate::model::PROVIDERS {
            assert!(
                FETCHES_IN_PROCESS.contains(&p.name) || known_passthrough.contains(&p.name),
                "provider '{}' is not classified in fetch.rs",
                p.name
            );
        }
    }

    fn policy(allow_private: bool) -> FetchPolicy {
        FetchPolicy { allow_private, max_bytes: 64 }
    }

    async fn fetch(p: &FetchPolicy, url: &str) -> Result<Fetched> {
        fetch_with(p, "test", url, &Deadline::new(10.0), Retry::new(0)).await
    }

    #[tokio::test]
    async fn refuses_bad_schemes_and_private_literals_before_connecting() {
        let p = policy(false);
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/a.pdf",
            "gopher://x/",
            "http://127.0.0.1:9/a.pdf",
            "http://[::1]/a.pdf",
            "http://[::ffff:127.0.0.1]/a.pdf",
            "http://169.254.169.254/latest/meta-data/",
            "http://2130706433/a.pdf", // 127.0.0.1 as a decimal integer
            "http://0x7f.1/a.pdf",
            "http://10.0.0.1/a.pdf",
            "http://user:pw@example.com/a.pdf",
        ] {
            let e = fetch(&p, url).await.unwrap_err();
            assert_eq!(e.kind, ErrorKind::Input, "{url}: {e}");
        }
    }

    #[tokio::test]
    async fn refuses_names_that_resolve_to_private_addresses() {
        let e = fetch(&policy(false), "http://localhost:9/a.pdf").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Input, "{e}");
        assert!(e.message.contains("non-public"), "{e}");
    }

    /// A loopback server answering each connection with the next raw response.
    async fn raw_server(responses: Vec<String>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for r in responses {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(r.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        base
    }

    fn ok(body: &str, ct: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect(to: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
    }

    #[tokio::test]
    async fn trusted_policy_downloads_and_follows_redirects() {
        let base = raw_server(vec![redirect("/b.pdf"), ok("%PDF-1.4", "application/pdf; charset=binary")]).await;
        let f = fetch(&policy(true), &format!("{base}/a.pdf")).await.unwrap();
        assert_eq!(&f.data[..], b"%PDF-1.4");
        assert_eq!(f.content_type.as_deref(), Some("application/pdf"));
    }

    #[tokio::test]
    async fn redirect_hops_are_revalidated() {
        // Every hop target goes through the same check as the first URL.
        let p = policy(false);
        let e = check_url(&p, &url::Url::parse("http://example.com/x").unwrap().join("http://127.0.0.1/y").unwrap());
        assert_eq!(e.unwrap_err().kind, ErrorKind::Input);
        // And a chain longer than MAX_REDIRECTS stops.
        let hops: Vec<String> = (0..=MAX_REDIRECTS).map(|i| redirect(&format!("/{i}"))).collect();
        let base = raw_server(hops).await;
        let e = fetch(&policy(true), &format!("{base}/start")).await.unwrap_err();
        assert!(e.message.contains("redirected more than"), "{e}");
    }

    #[tokio::test]
    async fn enforces_the_size_cap_and_hides_error_bodies() {
        let big = "x".repeat(100);
        let base = raw_server(vec![ok(&big, "application/pdf")]).await;
        let e = fetch(&policy(true), &format!("{base}/big.pdf")).await.unwrap_err();
        assert!(e.message.contains("download limit"), "{e}");
        // Chunked, no content-length: still stopped while streaming.
        let chunked = format!(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n64\r\n{big}\r\n0\r\n\r\n"
        );
        let base = raw_server(vec![chunked]).await;
        let e = fetch(&policy(true), &format!("{base}/big.pdf")).await.unwrap_err();
        assert!(e.message.contains("download limit"), "{e}");
        // An error page's content never reaches the message.
        let secret = "HTTP/1.1 404 Not Found\r\ncontent-length: 21\r\nconnection: close\r\n\r\nINTERNAL-SECRET-TOKEN";
        let base = raw_server(vec![secret.to_string()]).await;
        let e = fetch(&policy(true), &format!("{base}/missing.pdf")).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Input);
        assert!(e.message.contains("HTTP 404") && !e.message.contains("SECRET"), "{e}");
    }

    #[test]
    fn env_policy_defaults_to_safe() {
        let p = FetchPolicy::default();
        assert!(!p.allow_private);
        assert_eq!(p.max_bytes, 50 * 1024 * 1024);
    }
}
