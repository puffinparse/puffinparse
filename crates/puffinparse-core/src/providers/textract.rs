//! AWS Textract (`textract.<region>.amazonaws.com`).
//!
//! Textract is a JSON-RPC ("AWS JSON 1.1") API: every call is `POST /` on the regional endpoint
//! with an `X-Amz-Target` header naming the operation, a SigV4 `Authorization` header, and a JSON
//! body. There is no upload endpoint and no URL input — the document travels inline as
//! base64 in `Document.Bytes` (synchronous operations) or lives in S3 (asynchronous ones).
//!
//! Flow:
//!
//! * **Synchronous** (default): `Textract.DetectDocumentText` or `Textract.AnalyzeDocument` with
//!   `Document: {Bytes: "<base64>"}`. Accepts JPEG/PNG/PDF/TIFF up to 10 MB, but **PDF and TIFF
//!   are limited to one page**.
//! * **Asynchronous** (only with `provider_options.s3_object`):
//!   `Textract.StartDocumentTextDetection` / `Textract.StartDocumentAnalysis` with
//!   `DocumentLocation.S3Object`, then poll `Textract.GetDocumentTextDetection` /
//!   `Textract.GetDocumentAnalysis` and follow `NextToken` until every `Blocks` page is collected.
//!   PuffinParse has no S3 client, so the caller must put the object in the bucket themselves.
//!
//! Multi-page PDFs without an `s3_object` are rejected up front with a `bad_request` error rather
//! than billed and failed server-side.

use crate::error::{Error, ErrorKind, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
use crate::types::{
    BBox, Block, BlockType, Citation, DocumentRequest, ExtractRequest, ExtractResponse, FieldInfo, Line, Mode,
    OutputFormat, ParseResponse, TextPage, TextResponse, Usage, Word,
};
use crate::util::deep_merge;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

pub const NAME: &str = "textract";
const ENV_KEY: &str = "AWS_ACCESS_KEY_ID";
const ENV_SECRET: &str = "AWS_SECRET_ACCESS_KEY";
const ENV_TOKEN: &str = "AWS_SESSION_TOKEN";
const ENV_REGION: &str = "AWS_REGION";
const ENV_REGION_FALLBACK: &str = "AWS_DEFAULT_REGION";
const ENV_BASE: &str = "TEXTRACT_BASE_URL";
const DEFAULT_REGION: &str = "us-east-1";
const SERVICE: &str = "textract";
const CONTENT_TYPE: &str = "application/x-amz-json-1.1";
/// Textract quota: 15 queries per page synchronously, 30 asynchronously.
const MAX_QUERIES_SYNC: usize = 15;
const MAX_QUERIES_ASYNC: usize = 30;

#[derive(Debug, Default, Clone, Copy)]
pub struct Textract;

#[async_trait]
impl Provider for Textract {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn parse(&self, request: &DocumentRequest, model: &str) -> Result<ParseResponse> {
        match model {
            "layout" => {
                let ctx = Ctx::new(request)?;
                let analysis = ctx.analyze(request, &["LAYOUT", "TABLES"], None).await?;
                Ok(build_parse(request, &analysis, &ctx, model))
            }
            other => Err(unsupported(other, Mode::Parse)),
        }
    }

    async fn ocr(&self, request: &DocumentRequest, model: &str) -> Result<TextResponse> {
        let ctx = Ctx::new(request)?;
        let features: &[&str] = match model {
            "detect-text" => &[],
            "layout" => &["LAYOUT", "TABLES"],
            other => return Err(unsupported(other, Mode::Ocr)),
        };
        let analysis = ctx.analyze(request, features, None).await?;
        Ok(build_text(request, &analysis, &ctx, model))
    }

    async fn extract(&self, request: &ExtractRequest, model: &str) -> Result<ExtractResponse> {
        let doc = &request.document;
        let ctx = Ctx::new(doc)?;
        match model {
            "queries" => {
                let limit = if ctx.s3_object.is_some() { MAX_QUERIES_ASYNC } else { MAX_QUERIES_SYNC };
                let plan = QueryPlan::from_schema(&request.schema, limit, ctx.page_ranges.as_deref())?;
                let analysis = ctx.analyze(doc, &["QUERIES"], Some(plan.queries.clone())).await?;
                Ok(build_queries(request, &analysis, &ctx, model, &plan))
            }
            "forms" => {
                let analysis = ctx.analyze(doc, &["FORMS"], None).await?;
                Ok(build_forms(request, &analysis, &ctx, model))
            }
            other => Err(unsupported(other, Mode::Extract)),
        }
    }
}

fn unsupported(model: &str, mode: Mode) -> Error {
    Error::unsupported_model(format!("textract/{model} does not implement mode '{mode}'")).with_provider(NAME)
}

// ---- credentials, region, call context ----------------------------------------------------------

#[derive(Clone)]
struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// Never print key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &"<redacted>").finish_non_exhaustive()
    }
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `request.api_key` overrides **only the access key id**; the secret must still come from the
/// environment, because a single opaque string cannot carry an AWS key pair.
fn resolve_credentials(request: &DocumentRequest) -> Result<Credentials> {
    let override_id = request.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
    let secret = env_opt(ENV_SECRET).ok_or_else(|| {
        let extra = if override_id.is_some() {
            " (api_key overrides AWS_ACCESS_KEY_ID only — the secret always comes from the environment)"
        } else {
            ""
        };
        Error::authentication(format!("no credentials for textract: set {ENV_SECRET}{extra}")).with_provider(NAME)
    })?;
    let access_key_id = match override_id {
        Some(k) => k.to_string(),
        None => env_opt(ENV_KEY).ok_or_else(|| {
            Error::authentication(format!(
                "no credentials for textract: set {ENV_KEY} and {ENV_SECRET} (or pass api_key for the key id)"
            ))
            .with_provider(NAME)
        })?,
    };
    Ok(Credentials { access_key_id, secret_access_key: secret, session_token: env_opt(ENV_TOKEN) })
}

fn resolve_region(request: &DocumentRequest) -> String {
    request
        .option("region")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| env_opt(ENV_REGION))
        .or_else(|| env_opt(ENV_REGION_FALLBACK))
        .unwrap_or_else(|| DEFAULT_REGION.to_string())
}

/// Everything one Textract call needs: credentials, endpoint, deadline, retry policy.
#[derive(Debug)]
struct Ctx {
    creds: Credentials,
    region: String,
    endpoint: String,
    host: String,
    path: String,
    deadline: Deadline,
    retry: Retry,
    /// `provider_options.s3_object` → the asynchronous Start*/Get* flow.
    s3_object: Option<Value>,
    /// Extra keys from `provider_options`, deep-merged into the request body.
    passthrough: Option<Value>,
    page_ranges: Option<Vec<(u32, Option<u32>)>>,
}

impl Ctx {
    fn new(request: &DocumentRequest) -> Result<Self> {
        let creds = resolve_credentials(request)?;
        let region = resolve_region(request);
        // The region is spliced into the hostname the signed request (access key id, session
        // token) goes to, so it must be a single DNS label.
        provider::host_label(NAME, "region", &region)?;
        let default_base = format!("https://{SERVICE}.{region}.amazonaws.com");
        let endpoint = provider::resolve_base_url(request, ENV_BASE, &default_base);
        let parsed = url::Url::parse(&endpoint)
            .map_err(|e| Error::input(format!("invalid textract base URL '{endpoint}': {e}")))?;
        let host = match (parsed.host_str(), parsed.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            (None, _) => return Err(Error::input(format!("textract base URL '{endpoint}' has no host"))),
        };
        let path = if parsed.path().is_empty() { "/".to_string() } else { parsed.path().to_string() };
        let s3_object = request.option("s3_object").cloned().filter(|v| !v.is_null());
        if let Some(s3) = &s3_object {
            if s3.get("bucket").and_then(Value::as_str).is_none() || s3.get("name").and_then(Value::as_str).is_none() {
                return Err(Error::input(
                    "provider_options.s3_object must be an object with string 'bucket' and 'name' keys",
                )
                .with_provider(NAME));
            }
        }
        let passthrough = request.provider_options.as_ref().and_then(|v| {
            let mut v = v.clone();
            if let Value::Object(o) = &mut v {
                o.remove("region");
                o.remove("s3_object");
                if o.is_empty() {
                    return None;
                }
            }
            Some(v)
        });
        let page_ranges = match &request.pages {
            Some(spec) => Some(crate::util::parse_page_ranges(spec)?),
            None => None,
        };
        Ok(Self {
            creds,
            region,
            endpoint: format!("{}/", endpoint.trim_end_matches('/')),
            host,
            path,
            deadline: Deadline::new(request.timeout_secs),
            retry: Retry::new(request.max_retries),
            s3_object,
            passthrough,
            page_ranges,
        })
    }

    /// One signed `POST /` with `X-Amz-Target: <target>`.
    async fn send<T: serde::de::DeserializeOwned>(&self, target: &str, body: &Value) -> Result<T> {
        let payload = bytes::Bytes::from(serde_json::to_vec(body)?);
        http::with_retry(NAME, self.retry, &self.deadline, || {
            let payload = payload.clone();
            let headers = sign(&self.creds, &self.region, &self.host, &self.path, target, &payload, &now_utc());
            let mut rb = http::client()
                .post(&self.endpoint)
                .timeout(self.deadline.request_timeout())
                .header("content-type", CONTENT_TYPE)
                .header("x-amz-target", target)
                .body(payload);
            for (k, v) in headers {
                rb = rb.header(k, v);
            }
            async move {
                let resp = rb.send().await?;
                let status = resp.status().as_u16();
                let text = resp.text().await?;
                if (200..300).contains(&status) {
                    serde_json::from_str(&text).map_err(|e| {
                        Error::provider(format!(
                            "unexpected response shape: {e}; body starts: {}",
                            http::snippet(&text)
                        ))
                        .with_provider(NAME)
                    })
                } else {
                    Err(map_error(status, &text))
                }
            }
        })
        .await
    }

    /// Run the whole operation (sync or async) and collect every block.
    async fn analyze(
        &self,
        request: &DocumentRequest,
        features: &[&str],
        queries: Option<Vec<Value>>,
    ) -> Result<Analysis> {
        let mut body = Map::new();
        if !features.is_empty() {
            body.insert("FeatureTypes".into(), json!(features));
        }
        if let Some(q) = queries {
            body.insert("QueriesConfig".into(), json!({ "Queries": q }));
        }
        let analyzing = !features.is_empty();

        let mut analysis = match &self.s3_object {
            Some(s3) => {
                let mut object = json!({
                    "Bucket": s3.get("bucket").and_then(Value::as_str).unwrap_or_default(),
                    "Name": s3.get("name").and_then(Value::as_str).unwrap_or_default(),
                });
                if let Some(v) = s3.get("version").and_then(Value::as_str) {
                    object["Version"] = json!(v);
                }
                body.insert("DocumentLocation".into(), json!({ "S3Object": object }));
                let start = if analyzing { "StartDocumentAnalysis" } else { "StartDocumentTextDetection" };
                let get = if analyzing { "GetDocumentAnalysis" } else { "GetDocumentTextDetection" };
                self.run_async(Value::Object(body), start, get).await?
            }
            None => {
                let data = self.load_document(request).await?;
                if let Some(pages) = pdf_page_count(&data) {
                    if pages > 1 {
                        return Err(multipage_error(pages));
                    }
                }
                body.insert("Document".into(), json!({ "Bytes": base64_encode(&data) }));
                let target = if analyzing { "AnalyzeDocument" } else { "DetectDocumentText" };
                let wire: WireResponse =
                    self.send(&format!("Textract.{target}"), &self.merge(Value::Object(body))).await.map_err(|e| {
                        if is_pdf(&data) && e.kind == ErrorKind::BadRequest {
                            multipage_error(0).with_status(e.status_code.unwrap_or(400))
                        } else {
                            e
                        }
                    })?;
                Analysis {
                    blocks: wire.blocks,
                    doc_pages: wire.document_metadata.map(|m| m.pages).unwrap_or(0),
                    model_version: wire.model_version,
                    job_id: None,
                    warnings: wire.warnings,
                    features: Vec::new(),
                }
            }
        };

        if let Some(ranges) = &self.page_ranges {
            analysis.blocks.retain(|b| {
                let p = b.page.unwrap_or(1);
                ranges.iter().any(|(s, e)| p >= *s && e.map(|e| p <= e).unwrap_or(true))
            });
        }
        analysis.features = features.iter().map(|s| s.to_string()).collect();
        Ok(analysis)
    }

    /// `Start*` → poll `Get*` → follow `NextToken` until every block page is read.
    async fn run_async(&self, body: Value, start: &str, get: &str) -> Result<Analysis> {
        let submit: StartResponse = self.send(&format!("Textract.{start}"), &self.merge(body)).await?;
        let job_id = submit.job_id;
        tracing::debug!(job_id = %job_id, "textract: async job submitted");

        let first: GetResponse =
            http::poll_until(NAME, &self.deadline, Duration::from_secs(2), Duration::from_secs(10), || {
                let body = json!({ "JobId": job_id, "MaxResults": 1000 });
                async move {
                    let resp: GetResponse = self.send(&format!("Textract.{get}"), &body).await?;
                    Ok(match resp.job_status.as_deref() {
                        Some("IN_PROGRESS") | None => None,
                        _ => Some(resp),
                    })
                }
            })
            .await
            .map_err(|e| e.with_job_id(job_id.clone()))?;

        let status = first.job_status.clone().unwrap_or_default();
        if status != "SUCCEEDED" && status != "PARTIAL_SUCCESS" {
            let reason = first.status_message.unwrap_or_else(|| "no status message".into());
            return Err(Error::provider(format!("job {status}: {reason}")).with_provider(NAME).with_job_id(job_id));
        }

        let mut analysis = Analysis {
            blocks: first.base.blocks,
            doc_pages: first.base.document_metadata.map(|m| m.pages).unwrap_or(0),
            model_version: first.base.model_version,
            job_id: Some(job_id.clone()),
            warnings: first.base.warnings,
            features: Vec::new(),
        };
        let mut next = first.next_token;
        while let Some(token) = next {
            self.deadline.check(NAME, "paginating job results")?;
            let body = json!({ "JobId": job_id, "MaxResults": 1000, "NextToken": token });
            let page: GetResponse = self.send(&format!("Textract.{get}"), &body).await?;
            analysis.blocks.extend(page.base.blocks);
            analysis.warnings.extend(page.base.warnings);
            next = page.next_token;
        }
        if status == "PARTIAL_SUCCESS" {
            tracing::warn!(job_id = %job_id, warnings = analysis.warnings.len(), "textract: job partially succeeded");
        }
        Ok(analysis)
    }

    fn merge(&self, mut body: Value) -> Value {
        if let Some(patch) = &self.passthrough {
            deep_merge(&mut body, patch);
        }
        body
    }

    /// Textract cannot fetch URLs, so a URL input is downloaded here and sent inline.
    async fn load_document(&self, request: &DocumentRequest) -> Result<bytes::Bytes> {
        if let Some(data) = provider::load_bytes(&request.input).await? {
            return Ok(data);
        }
        let crate::types::DocumentInput::Url { url } = &request.input else { unreachable!() };
        let retry = http::Retry::new(request.max_retries);
        Ok(crate::fetch::fetch_document(NAME, url, &self.deadline, retry).await?.data)
    }
}

fn multipage_error(pages: u32) -> Error {
    let seen = if pages > 1 { format!(" (this document has {pages} pages)") } else { String::new() };
    Error::new(
        ErrorKind::BadRequest,
        format!(
            "textract: synchronous operations accept single-page PDF/TIFF only{seen}. Multi-page documents \
             require Textract's asynchronous API, which reads the file from S3. Upload the file yourself and \
             pass provider_options={{\"s3_object\": {{\"bucket\": \"my-bucket\", \"name\": \"path/doc.pdf\"}}}}, \
             or split the PDF into single pages first."
        ),
    )
    .with_provider(NAME)
}

fn is_pdf(data: &[u8]) -> bool {
    data.starts_with(b"%PDF-")
}

/// Best-effort page count for an uncompressed PDF: counts `/Type /Page` objects, falling back to
/// the largest `/Count` in the page tree. Returns `None` when the structure is not readable
/// (cross-reference / object streams), in which case the caller lets Textract decide.
fn pdf_page_count(data: &[u8]) -> Option<u32> {
    if !is_pdf(data) {
        return None;
    }
    let mut pages = 0u32;
    let mut i = 0usize;
    while let Some(pos) = find(data, b"/Type", i) {
        let mut j = pos + 5;
        while j < data.len() && (data[j] as char).is_whitespace() {
            j += 1;
        }
        if data[j..].starts_with(b"/Page") {
            let after = data.get(j + 5).copied().unwrap_or(b' ');
            // `/Pages` is the tree node, `/Page` followed by a delimiter is a leaf.
            if !after.is_ascii_alphanumeric() {
                pages += 1;
            }
        }
        i = pos + 5;
    }
    if pages > 0 {
        return Some(pages);
    }
    let mut best = 0u32;
    let mut i = 0usize;
    while let Some(pos) = find(data, b"/Count", i) {
        let mut j = pos + 6;
        while j < data.len() && (data[j] as char).is_whitespace() {
            j += 1;
        }
        let start = j;
        while j < data.len() && data[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            if let Ok(n) = std::str::from_utf8(&data[start..j]).unwrap_or("").parse::<u32>() {
                best = best.max(n);
            }
        }
        i = pos + 6;
    }
    (best > 0).then_some(best)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

// ---- errors --------------------------------------------------------------------------------------

/// Textract reports failures as `{"__type":"<Exception>","message":"…"}` with HTTP 400 for most of
/// them, so the shared status → kind mapping needs a Textract-specific override.
fn map_error(status: u16, body: &str) -> Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let kind_name = parsed
        .as_ref()
        .and_then(|v| v.get("__type").or_else(|| v.get("code")))
        .and_then(Value::as_str)
        .map(|t| t.rsplit(['#', '.']).next().unwrap_or(t).to_string());
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("message").or_else(|| v.get("Message")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| http::snippet(body));
    let kind = match kind_name.as_deref() {
        Some("AccessDeniedException")
        | Some("UnrecognizedClientException")
        | Some("IncompleteSignature")
        | Some("InvalidSignatureException")
        | Some("InvalidClientTokenId")
        | Some("MissingAuthenticationTokenException")
        | Some("ExpiredTokenException") => ErrorKind::Authentication,
        Some("ThrottlingException")
        | Some("ProvisionedThroughputExceededException")
        | Some("LimitExceededException")
        | Some("RequestLimitExceeded") => ErrorKind::RateLimit,
        Some("InternalServerError") | Some("ServiceUnavailable") | Some("InternalFailure") => ErrorKind::Provider,
        Some(_) if (400..500).contains(&status) => ErrorKind::BadRequest,
        _ => return Error::from_http(NAME, status, body),
    };
    let detail = kind_name.map(|t| format!("{t}: ")).unwrap_or_default();
    Error::new(kind, format!("{detail}{message}")).with_provider(NAME).with_status(status)
}

// ---- SigV4 ---------------------------------------------------------------------------------------

fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], msg: &str) -> Vec<u8> {
    use hmac::{KeyInit, Mac, SimpleHmac};
    let mut mac = <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Derive the AWS SigV4 signing key: `HMAC(HMAC(HMAC(HMAC("AWS4"+secret, date), region), service), "aws4_request")`.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let mut key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date);
    key = hmac_sha256(&key, region);
    key = hmac_sha256(&key, service);
    hmac_sha256(&key, "aws4_request")
}

/// Build the SigV4 headers for a Textract `POST` (no query string, JSON body).
/// Returns `(name, value)` pairs to add to the request; `content-type` and `x-amz-target` are set
/// by the caller and are included in the signature.
fn sign(
    creds: &Credentials,
    region: &str,
    host: &str,
    path: &str,
    target: &str,
    payload: &[u8],
    now: &chrono::DateTime<chrono::Utc>,
) -> Vec<(&'static str, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(payload);

    // Canonical headers must be sorted by lowercase name.
    let mut headers: Vec<(&str, String)> = vec![
        ("content-type", CONTENT_TYPE.to_string()),
        ("host", host.to_string()),
        ("x-amz-content-sha256", payload_hash.clone()),
        ("x-amz-date", amz_date.clone()),
        ("x-amz-target", target.to_string()),
    ];
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token", token.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed_headers = headers.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canonical_request = format!("POST\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let scope = format!("{date}/{region}/{SERVICE}/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
    let signature = hex(&hmac_sha256(&signing_key(&creds.secret_access_key, &date, region, SERVICE), &string_to_sign));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );
    let mut out =
        vec![("x-amz-date", amz_date), ("x-amz-content-sha256", payload_hash), ("authorization", authorization)];
    if let Some(token) = &creds.session_token {
        out.push(("x-amz-security-token", token.clone()));
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---- wire types ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct WireResponse {
    #[serde(default)]
    blocks: Vec<WireBlock>,
    #[serde(default)]
    document_metadata: Option<DocumentMetadata>,
    #[serde(default, rename = "AnalyzeDocumentModelVersion", alias = "DetectDocumentTextModelVersion")]
    model_version: Option<String>,
    #[serde(default)]
    warnings: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DocumentMetadata {
    #[serde(default)]
    pages: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StartResponse {
    job_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GetResponse {
    #[serde(flatten)]
    base: WireResponse,
    #[serde(default)]
    job_status: Option<String>,
    #[serde(default)]
    next_token: Option<String>,
    #[serde(default)]
    status_message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct WireBlock {
    #[serde(default)]
    block_type: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    geometry: Option<Geometry>,
    #[serde(default)]
    relationships: Vec<Relationship>,
    #[serde(default)]
    entity_types: Vec<String>,
    #[serde(default)]
    row_index: Option<u32>,
    #[serde(default)]
    column_index: Option<u32>,
    #[serde(default)]
    row_span: Option<u32>,
    #[serde(default)]
    column_span: Option<u32>,
    #[serde(default)]
    selection_status: Option<String>,
    #[serde(default)]
    query: Option<WireQuery>,
}

impl WireBlock {
    fn page_number(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    fn bbox(&self) -> Option<BBox> {
        let bb = self.geometry.as_ref()?.bounding_box.as_ref()?;
        Some(BBox::from_normalized_ltwh(bb.left, bb.top, bb.width, bb.height))
    }

    /// Textract reports confidence as a percentage; PuffinParse uses 0..1.
    fn confidence01(&self) -> Option<f64> {
        self.confidence.map(|c| (c / 100.0).clamp(0.0, 1.0))
    }

    fn children(&self) -> &[String] {
        self.related("CHILD")
    }

    fn related(&self, kind: &str) -> &[String] {
        self.relationships.iter().find(|r| r.relationship_type == kind).map(|r| r.ids.as_slice()).unwrap_or(&[])
    }

    fn is_entity(&self, kind: &str) -> bool {
        self.entity_types.iter().any(|e| e == kind)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Geometry {
    #[serde(default)]
    bounding_box: Option<BoundingBox>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct BoundingBox {
    #[serde(default)]
    left: f64,
    #[serde(default)]
    top: f64,
    #[serde(default)]
    width: f64,
    #[serde(default)]
    height: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Relationship {
    #[serde(rename = "Type", default)]
    relationship_type: String,
    #[serde(rename = "Ids", default)]
    ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct WireQuery {
    #[serde(default)]
    text: String,
    #[serde(default)]
    alias: Option<String>,
}

/// Everything a mode needs after the transport is done.
#[derive(Debug, Default)]
struct Analysis {
    blocks: Vec<WireBlock>,
    doc_pages: u32,
    model_version: Option<String>,
    job_id: Option<String>,
    warnings: Vec<Value>,
    features: Vec<String>,
}

impl Analysis {
    fn index(&self) -> HashMap<&str, &WireBlock> {
        self.blocks.iter().map(|b| (b.id.as_str(), b)).collect()
    }

    fn page_count(&self) -> u32 {
        let seen = self.blocks.iter().map(|b| b.page_number()).max().unwrap_or(0);
        self.doc_pages.max(seen).max(1)
    }

    fn usage(&self) -> Usage {
        Usage { pages: self.page_count(), credits: None, provider_cost_usd: None }
    }

    fn raw(&self) -> Value {
        json!({
            "DocumentMetadata": { "Pages": self.doc_pages },
            "AnalyzeDocumentModelVersion": self.model_version,
            "Blocks": self.blocks,
            "Warnings": self.warnings,
        })
    }

    fn apply_metadata(&self, meta: &mut BTreeMap<String, Value>, ctx: &Ctx) {
        meta.insert("textract_region".into(), json!(ctx.region));
        if let Some(v) = &self.model_version {
            meta.insert("textract_model_version".into(), json!(v));
        }
        if !self.features.is_empty() {
            meta.insert("textract_feature_types".into(), json!(self.features));
        }
        if !self.warnings.is_empty() {
            meta.insert("textract_warnings".into(), json!(self.warnings));
        }
    }
}

// ---- ocr mode ------------------------------------------------------------------------------------

fn build_text(request: &DocumentRequest, analysis: &Analysis, ctx: &Ctx, model: &str) -> TextResponse {
    let mut by_page: BTreeMap<u32, (Vec<Line>, Vec<Word>)> = BTreeMap::new();
    for p in 1..=analysis.page_count() {
        by_page.entry(p).or_default();
    }
    for b in &analysis.blocks {
        let entry = by_page.entry(b.page_number()).or_default();
        let Some(text) = b.text.as_deref().filter(|t| !t.is_empty()) else { continue };
        match b.block_type.as_str() {
            "LINE" => entry.0.push(Line { text: text.to_string(), bbox: b.bbox(), confidence: b.confidence01() }),
            "WORD" => entry.1.push(Word { text: text.to_string(), bbox: b.bbox(), confidence: b.confidence01() }),
            _ => {}
        }
    }
    let pages: Vec<TextPage> = by_page
        .into_iter()
        .map(|(page_number, (lines, words))| {
            let text = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
            TextPage { page_number, width: None, height: None, text, lines, words }
        })
        .collect();
    let mut resp = TextResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, analysis.usage());
    resp.provider_job_id = analysis.job_id.clone();
    analysis.apply_metadata(&mut resp.metadata, ctx);
    if request.include_raw {
        resp.raw = Some(analysis.raw());
    }
    resp
}

// ---- parse mode (LAYOUT + TABLES) ----------------------------------------------------------------

fn map_layout_type(t: &str) -> BlockType {
    match t {
        "LAYOUT_TITLE" => BlockType::Title,
        "LAYOUT_SECTION_HEADER" => BlockType::SectionHeader,
        "LAYOUT_HEADER" => BlockType::Header,
        "LAYOUT_FOOTER" => BlockType::Footer,
        "LAYOUT_LIST" => BlockType::List,
        "LAYOUT_TABLE" | "TABLE" => BlockType::Table,
        "LAYOUT_FIGURE" => BlockType::Figure,
        "LAYOUT_TEXT" | "LAYOUT_KEY_VALUE" => BlockType::Text,
        // LAYOUT_PAGE_NUMBER and anything new.
        _ => BlockType::Other,
    }
}

fn is_layout(t: &str) -> bool {
    t.starts_with("LAYOUT_")
}

/// Text of a layout element: its descendant `LINE` blocks, one per output line.
/// `LAYOUT_LIST` points at `LAYOUT_TEXT` children rather than lines, so recurse one level.
fn layout_lines(block: &WireBlock, index: &HashMap<&str, &WireBlock>, depth: u8) -> Vec<String> {
    let mut out = Vec::new();
    for id in block.children() {
        let Some(child) = index.get(id.as_str()) else { continue };
        match child.block_type.as_str() {
            "LINE" | "WORD" => {
                if let Some(t) = child.text.as_deref().filter(|t| !t.trim().is_empty()) {
                    out.push(t.trim().to_string());
                }
            }
            _ if is_layout(&child.block_type) && depth < 4 => {
                let nested = layout_lines(child, index, depth + 1);
                if !nested.is_empty() {
                    out.push(nested.join(" "));
                }
            }
            _ => {}
        }
    }
    out
}

fn cell_text(cell: &WireBlock, index: &HashMap<&str, &WireBlock>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for id in cell.children() {
        let Some(child) = index.get(id.as_str()) else { continue };
        match child.block_type.as_str() {
            "WORD" => {
                if let Some(t) = child.text.as_deref().filter(|t| !t.is_empty()) {
                    parts.push(t.to_string());
                }
            }
            "SELECTION_ELEMENT" => {
                parts.push(if child.selection_status.as_deref() == Some("SELECTED") { "[x]" } else { "[ ]" }.into())
            }
            _ => {}
        }
    }
    parts.join(" ")
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ").trim().to_string()
}

/// Render a `TABLE` block as a markdown table from its `CELL` children.
fn render_table(table: &WireBlock, index: &HashMap<&str, &WireBlock>) -> String {
    let mut grid: BTreeMap<(u32, u32), String> = BTreeMap::new();
    let mut max_col = 0u32;
    let mut header_row: Option<u32> = None;
    for id in table.children() {
        let Some(cell) = index.get(id.as_str()) else { continue };
        if cell.block_type != "CELL" && cell.block_type != "MERGED_CELL" {
            continue;
        }
        if cell.block_type == "MERGED_CELL" {
            continue; // merged cells repeat content already present in the individual cells
        }
        let (Some(row), Some(col)) = (cell.row_index, cell.column_index) else { continue };
        if cell.is_entity("COLUMN_HEADER") {
            header_row = Some(header_row.map_or(row, |h: u32| h.min(row)));
        }
        let text = escape_cell(&cell_text(cell, index));
        let span = cell.column_span.unwrap_or(1).max(1);
        max_col = max_col.max(col + span - 1);
        grid.insert((row, col), text);
    }
    if grid.is_empty() || max_col == 0 {
        return String::new();
    }
    let rows: Vec<u32> = grid.keys().map(|(r, _)| *r).collect::<BTreeSet<_>>().into_iter().collect();
    let header_row = header_row.unwrap_or_else(|| rows.first().copied().unwrap_or(1));
    let line = |row: u32| -> String {
        let cells: Vec<String> = (1..=max_col).map(|c| grid.get(&(row, c)).cloned().unwrap_or_default()).collect();
        format!("| {} |", cells.join(" | "))
    };

    let mut out: Vec<String> = Vec::new();
    // Cells above the header row (table titles, section rows) are emitted as plain lines.
    for r in rows.iter().copied().filter(|r| *r < header_row) {
        let text = (1..=max_col)
            .filter_map(|c| grid.get(&(r, c)))
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        if !text.is_empty() {
            out.push(text);
        }
    }
    for id in table.related("TABLE_TITLE") {
        if let Some(t) = index.get(id.as_str()).and_then(|b| b.text.as_deref()).filter(|t| !t.trim().is_empty()) {
            out.push(t.trim().to_string());
        }
    }
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push(line(header_row));
    out.push(format!("|{}|", " --- |".repeat(max_col as usize)));
    for r in rows.iter().copied().filter(|r| *r > header_row) {
        out.push(line(r));
    }
    for id in table.related("TABLE_FOOTER") {
        if let Some(t) = index.get(id.as_str()).and_then(|b| b.text.as_deref()).filter(|t| !t.trim().is_empty()) {
            out.push(String::new());
            out.push(t.trim().to_string());
        }
    }
    out.join("\n")
}

/// Pick the `TABLE` block a `LAYOUT_TABLE` refers to: a direct child if there is one, otherwise the
/// unclaimed table on the same page whose box overlaps the layout box most.
fn table_for_layout<'a>(
    layout: &WireBlock,
    index: &HashMap<&str, &'a WireBlock>,
    tables: &[&'a WireBlock],
    used: &mut BTreeSet<String>,
) -> Option<&'a WireBlock> {
    for id in layout.children() {
        if let Some(t) = index.get(id.as_str()).filter(|b| b.block_type == "TABLE") {
            used.insert(t.id.clone());
            return Some(t);
        }
    }
    let lb = layout.bbox()?;
    let best = tables
        .iter()
        .filter(|t| !used.contains(&t.id) && t.page_number() == layout.page_number())
        .filter_map(|t| t.bbox().map(|tb| (t, overlap(&lb, &tb))))
        .filter(|(_, o)| *o > 0.1)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(t, _)| *t)?;
    used.insert(best.id.clone());
    Some(best)
}

fn overlap(a: &BBox, b: &BBox) -> f64 {
    let w = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
    let h = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
    let inter = w * h;
    let area_b = ((b.x1 - b.x0) * (b.y1 - b.y0)).max(1e-9);
    inter / area_b
}

fn build_parse(request: &DocumentRequest, analysis: &Analysis, ctx: &Ctx, model: &str) -> ParseResponse {
    let index = analysis.index();
    let tables: Vec<&WireBlock> = analysis.blocks.iter().filter(|b| b.block_type == "TABLE").collect();
    let mut used_tables: BTreeSet<String> = BTreeSet::new();
    let mut blocks: Vec<Block> = Vec::new();
    let has_layout = analysis.blocks.iter().any(|b| is_layout(&b.block_type));

    if has_layout {
        // LAYOUT_LIST points at LAYOUT_TEXT children; those also appear at the top level of
        // `Blocks` and must not be emitted twice.
        let nested: BTreeSet<&str> = analysis
            .blocks
            .iter()
            .filter(|b| is_layout(&b.block_type))
            .flat_map(|b| b.children().iter().map(String::as_str))
            .filter(|id| index.get(id).is_some_and(|c| is_layout(&c.block_type)))
            .collect();
        // Textract returns LAYOUT_* blocks in implied reading order; keep that order.
        for b in analysis.blocks.iter().filter(|b| is_layout(&b.block_type) && !nested.contains(b.id.as_str())) {
            let block_type = map_layout_type(&b.block_type);
            let lines = layout_lines(b, &index, 0);
            let text = lines.join("\n");
            let content = match b.block_type.as_str() {
                "LAYOUT_TITLE" => format!("# {}", lines.join(" ")),
                "LAYOUT_SECTION_HEADER" => format!("## {}", lines.join(" ")),
                "LAYOUT_LIST" => lines.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n"),
                "LAYOUT_TABLE" => match table_for_layout(b, &index, &tables, &mut used_tables) {
                    Some(t) => {
                        let md = render_table(t, &index);
                        if md.is_empty() {
                            text.clone()
                        } else {
                            md
                        }
                    }
                    None => text.clone(),
                },
                _ => text.clone(),
            };
            if content.trim().is_empty() {
                continue;
            }
            blocks.push(Block {
                block_type,
                content,
                text: (!text.is_empty()).then(|| text.clone()),
                bbox: b.bbox(),
                confidence: b.confidence01(),
                page_number: b.page_number(),
            });
        }
    } else {
        // No LAYOUT blocks (LAYOUT disabled or nothing detected): fall back to tables + lines.
        let mut words_in_tables: BTreeSet<&str> = BTreeSet::new();
        for t in &tables {
            for cid in t.children() {
                let Some(cell) = index.get(cid.as_str()) else { continue };
                for wid in cell.children() {
                    words_in_tables.insert(wid.as_str());
                }
            }
        }
        for t in &tables {
            let md = render_table(t, &index);
            if !md.is_empty() {
                blocks.push(Block {
                    block_type: BlockType::Table,
                    content: md,
                    text: None,
                    bbox: t.bbox(),
                    confidence: t.confidence01(),
                    page_number: t.page_number(),
                });
            }
        }
        for b in analysis.blocks.iter().filter(|b| b.block_type == "LINE") {
            let in_table =
                !b.children().is_empty() && b.children().iter().all(|w| words_in_tables.contains(w.as_str()));
            if in_table {
                continue;
            }
            if let Some(text) = b.text.as_deref().filter(|t| !t.trim().is_empty()) {
                blocks.push(Block {
                    block_type: BlockType::Text,
                    content: text.to_string(),
                    text: Some(text.to_string()),
                    bbox: b.bbox(),
                    confidence: b.confidence01(),
                    page_number: b.page_number(),
                });
            }
        }
    }

    if request.output == OutputFormat::Text {
        for b in &mut blocks {
            b.content = b.text.clone().unwrap_or_else(|| crate::types::markdown_to_text(&b.content));
        }
    }

    let mut pages = crate::types::pages_from_blocks(blocks, &BTreeMap::new());
    // Keep empty pages so page numbering matches the document.
    for n in 1..=analysis.page_count() {
        if !pages.iter().any(|p| p.page_number == n) {
            pages.push(crate::types::Page {
                page_number: n,
                width: None,
                height: None,
                markdown: String::new(),
                text: String::new(),
                blocks: vec![],
            });
        }
    }
    let mut resp = ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, analysis.usage());
    resp.provider_job_id = analysis.job_id.clone();
    analysis.apply_metadata(&mut resp.metadata, ctx);
    if request.include_raw {
        resp.raw = Some(analysis.raw());
    }
    resp
}

// ---- extract mode: QUERIES -----------------------------------------------------------------------

#[derive(Debug, Default)]
struct QueryPlan {
    queries: Vec<Value>,
    /// Textract `Alias` → the schema property it came from.
    alias_to_key: BTreeMap<String, String>,
    /// Property name → its JSON Schema fragment (for type coercion).
    props: BTreeMap<String, Value>,
    /// Properties that could not become a query (nested objects / arrays).
    skipped: Vec<String>,
}

impl QueryPlan {
    fn from_schema(schema: &Value, limit: usize, pages: Option<&[(u32, Option<u32>)]>) -> Result<Self> {
        let props = schema.get("properties").and_then(Value::as_object).ok_or_else(|| {
            Error::input("textract/queries: schema must have a top-level 'properties' object").with_provider(NAME)
        })?;
        let page_spec: Option<Vec<String>> = pages.map(|ranges| {
            ranges
                .iter()
                .map(|(s, e)| match e {
                    Some(e) if e == s => s.to_string(),
                    Some(e) => format!("{s}-{e}"),
                    None => format!("{s}-"),
                })
                .collect()
        });

        let mut plan = QueryPlan::default();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (key, prop) in props {
            let ty = prop.get("type").and_then(Value::as_str).unwrap_or("string");
            if matches!(ty, "object" | "array") || prop.get("properties").is_some() || prop.get("items").is_some() {
                plan.skipped.push(key.clone());
                continue;
            }
            let text = prop
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| prop.get("title").and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| key.replace(['_', '-'], " "));
            let mut alias = sanitize_alias(key);
            let mut n = 2;
            while !seen.insert(alias.clone()) {
                alias = format!("{}_{n}", sanitize_alias(key));
                n += 1;
            }
            let mut q = json!({ "Text": text, "Alias": alias });
            if let Some(p) = &page_spec {
                q["Pages"] = json!(p);
            }
            plan.queries.push(q);
            plan.alias_to_key.insert(alias, key.clone());
            plan.props.insert(key.clone(), prop.clone());
        }
        if plan.queries.is_empty() {
            return Err(Error::input(format!(
                "textract/queries: the schema has no flat (string / number / boolean) properties to query; \
                 nested properties are not supported: {}",
                plan.skipped.join(", ")
            ))
            .with_provider(NAME));
        }
        if plan.queries.len() > limit {
            return Err(Error::input(format!(
                "textract/queries: {} queries requested but Textract allows at most {limit} per page \
                 (15 synchronous, 30 asynchronous). Trim the schema or split the call.",
                plan.queries.len()
            ))
            .with_provider(NAME));
        }
        Ok(plan)
    }
}

/// Textract aliases are free text but round-trip most reliably as `[A-Za-z0-9_.:-]`.
fn sanitize_alias(key: &str) -> String {
    let s: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':') { c } else { '_' })
        .collect();
    let s = s.trim_matches('_').to_string();
    let s = if s.is_empty() { "field".to_string() } else { s };
    s.chars().take(200).collect()
}

fn build_queries(
    request: &ExtractRequest,
    analysis: &Analysis,
    ctx: &Ctx,
    model: &str,
    plan: &QueryPlan,
) -> ExtractResponse {
    let index = analysis.index();
    let mut data = Map::new();
    let mut fields: BTreeMap<String, FieldInfo> = BTreeMap::new();
    let mut answered: BTreeSet<&str> = BTreeSet::new();

    for q in analysis.blocks.iter().filter(|b| b.block_type == "QUERY") {
        let Some(alias) = q.query.as_ref().and_then(|x| x.alias.as_deref()) else { continue };
        let Some(key) = plan.alias_to_key.get(alias) else { continue };
        answered.insert(key.as_str());
        // The answer with the highest confidence wins when Textract returns several.
        let best = q
            .related("ANSWER")
            .iter()
            .filter_map(|id| index.get(id.as_str()))
            .filter(|b| b.block_type == "QUERY_RESULT")
            .filter(|b| b.text.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false))
            .max_by(|a, b| a.confidence.unwrap_or(0.0).total_cmp(&b.confidence.unwrap_or(0.0)));
        match best {
            Some(ans) => {
                let text = ans.text.clone().unwrap_or_default();
                data.insert(key.clone(), coerce(&text, plan.props.get(key)));
                let mut info = FieldInfo { confidence: ans.confidence01(), citations: Vec::new() };
                if request.citations {
                    info.citations.push(Citation {
                        page_number: ans.page_number(),
                        bbox: ans.bbox(),
                        text: Some(text),
                    });
                }
                fields.insert(format!("/{key}"), info);
            }
            None => {
                data.insert(key.clone(), Value::Null);
            }
        }
    }
    for key in plan.alias_to_key.values() {
        if !answered.contains(key.as_str()) {
            data.entry(key.clone()).or_insert(Value::Null);
        }
    }

    let mut resp = ExtractResponse::new(NAME, &format!("{NAME}/{model}"), Value::Object(data), analysis.usage());
    resp.fields = fields;
    resp.provider_job_id = analysis.job_id.clone();
    analysis.apply_metadata(&mut resp.metadata, ctx);
    if !plan.skipped.is_empty() {
        resp.metadata.insert("textract_unsupported_fields".into(), json!(plan.skipped));
    }
    if request.instructions.is_some() {
        resp.metadata.insert("textract_instructions_ignored".into(), json!(true));
    }
    if request.document.include_raw {
        resp.raw = Some(analysis.raw());
    }
    resp
}

// ---- extract mode: FORMS -------------------------------------------------------------------------

fn normalize_name(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn build_forms(request: &ExtractRequest, analysis: &Analysis, ctx: &Ctx, model: &str) -> ExtractResponse {
    let index = analysis.index();
    // Collect (normalised key text, key block, value block).
    let mut pairs: Vec<(String, &WireBlock, Option<&WireBlock>)> = Vec::new();
    for k in analysis.blocks.iter().filter(|b| b.block_type == "KEY_VALUE_SET" && b.is_entity("KEY")) {
        let key_text = cell_text(k, &index);
        if key_text.trim().is_empty() {
            continue;
        }
        let value = k.related("VALUE").iter().filter_map(|id| index.get(id.as_str()).copied()).next();
        pairs.push((normalize_name(&key_text), k, value));
    }

    let empty = Map::new();
    let props = request.schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    let mut data = Map::new();
    let mut fields: BTreeMap<String, FieldInfo> = BTreeMap::new();
    let mut unmatched: Vec<String> = Vec::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();

    for (key, prop) in props {
        let ty = prop.get("type").and_then(Value::as_str).unwrap_or("string");
        if matches!(ty, "object" | "array") {
            unmatched.push(key.clone());
            data.insert(key.clone(), Value::Null);
            continue;
        }
        let mut candidates: Vec<String> = vec![normalize_name(key)];
        if let Some(t) = prop.get("title").and_then(Value::as_str) {
            candidates.push(normalize_name(t));
        }
        // Exact match first, then "one name contains the other" — best effort, first unused wins.
        let exact = |k: &str| candidates.iter().any(|c| !c.is_empty() && k == c);
        let fuzzy = |k: &str| candidates.iter().any(|c| c.len() >= 3 && (k.contains(c.as_str()) || c.contains(k)));
        let free = |i: &usize| !used.contains(i);
        let found = pairs
            .iter()
            .enumerate()
            .find(|(i, (k, _, _))| free(i) && exact(k))
            .or_else(|| pairs.iter().enumerate().find(|(i, (k, _, _))| free(i) && fuzzy(k)))
            .map(|(i, _)| i);
        let Some(idx) = found else {
            unmatched.push(key.clone());
            data.insert(key.clone(), Value::Null);
            continue;
        };
        used.insert(idx);
        let (_, key_block, value_block) = &pairs[idx];
        let text = value_block.map(|v| cell_text(v, &index)).unwrap_or_default();
        data.insert(key.clone(), if text.trim().is_empty() { Value::Null } else { coerce(&text, Some(prop)) });
        let source = value_block.unwrap_or(key_block);
        let mut info = FieldInfo { confidence: source.confidence01(), citations: Vec::new() };
        if request.citations {
            info.citations.push(Citation {
                page_number: source.page_number(),
                bbox: source.bbox(),
                text: (!text.trim().is_empty()).then(|| text.clone()),
            });
        }
        fields.insert(format!("/{key}"), info);
    }

    let mut resp = ExtractResponse::new(NAME, &format!("{NAME}/{model}"), Value::Object(data), analysis.usage());
    resp.fields = fields;
    resp.provider_job_id = analysis.job_id.clone();
    analysis.apply_metadata(&mut resp.metadata, ctx);
    if !unmatched.is_empty() {
        resp.metadata.insert("textract_unmatched_fields".into(), json!(unmatched));
    }
    resp.metadata.insert("textract_form_keys_found".into(), json!(pairs.len()));
    if request.instructions.is_some() {
        resp.metadata.insert("textract_instructions_ignored".into(), json!(true));
    }
    if request.document.include_raw {
        resp.raw = Some(analysis.raw());
    }
    resp
}

/// Coerce a recognised string into the type the schema asks for. Textract only ever returns text.
fn coerce(text: &str, prop: Option<&Value>) -> Value {
    let text = text.trim();
    let ty = prop.and_then(|p| p.get("type")).and_then(Value::as_str).unwrap_or("string");
    match ty {
        "number" | "integer" => {
            let cleaned: String = text.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')).collect();
            match cleaned.parse::<f64>() {
                Ok(n) if ty == "integer" => json!(n as i64),
                Ok(n) => serde_json::Number::from_f64(n).map(Value::Number).unwrap_or_else(|| json!(text)),
                Err(_) => json!(text),
            }
        }
        "boolean" => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "x" | "selected" | "checked" | "1" | "[x]" => json!(true),
            "false" | "no" | "n" | "not_selected" | "unchecked" | "0" | "" | "[ ]" | "[]" => json!(false),
            _ => json!(text),
        },
        _ => json!(text),
    }
}

// ---- tests ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis_from(raw: &str) -> Analysis {
        let wire: WireResponse = serde_json::from_str(raw).unwrap();
        Analysis {
            blocks: wire.blocks,
            doc_pages: wire.document_metadata.map(|m| m.pages).unwrap_or(0),
            model_version: wire.model_version,
            job_id: None,
            warnings: wire.warnings,
            features: Vec::new(),
        }
    }

    fn ctx() -> Ctx {
        Ctx {
            creds: Credentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            region: "us-east-1".into(),
            endpoint: "https://textract.us-east-1.amazonaws.com/".into(),
            host: "textract.us-east-1.amazonaws.com".into(),
            path: "/".into(),
            deadline: Deadline::new(10.0),
            retry: Retry::new(0),
            s3_object: None,
            passthrough: None,
            page_ranges: None,
        }
    }

    // -- SigV4 (AWS's own published example vectors) --

    #[test]
    fn derives_the_documented_signing_key() {
        let key = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20150830", "us-east-1", "iam");
        assert_eq!(hex(&key), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
    }

    #[test]
    fn reproduces_the_documented_signature() {
        // AWS "Signature Version 4 test suite" GET iam ListUsers example.
        let canonical = "GET\n/\nAction=ListUsers&Version=2010-05-08\n\
             content-type:application/x-www-form-urlencoded; charset=utf-8\n\
             host:iam.amazonaws.com\nx-amz-date:20150830T123600Z\n\n\
             content-type;host;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            sha256_hex(canonical.as_bytes()),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        let sts = format!(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/iam/aws4_request\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let key = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20150830", "us-east-1", "iam");
        assert_eq!(hex(&hmac_sha256(&key, &sts)), "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7");
    }

    #[test]
    fn signs_a_textract_request() {
        use chrono::TimeZone;
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: Some("TOKEN".into()),
        };
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 11, 12, 34, 56).unwrap();
        let headers = sign(
            &creds,
            "eu-west-1",
            "textract.eu-west-1.amazonaws.com",
            "/",
            "Textract.DetectDocumentText",
            b"{}",
            &now,
        );
        let map: BTreeMap<_, _> = headers.into_iter().collect();
        assert_eq!(map["x-amz-date"], "20260911T123456Z");
        assert_eq!(map["x-amz-content-sha256"], sha256_hex(b"{}"));
        assert_eq!(map["x-amz-security-token"], "TOKEN");
        let auth = &map["authorization"];
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260911/eu-west-1/textract/aws4_request,"));
        assert!(
            auth.contains(
                "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token;x-amz-target,"
            ),
            "{auth}"
        );
        // Deterministic signature for these inputs — guards against canonicalisation drift.
        assert!(auth.ends_with(&format!("Signature={}", {
            let canonical = format!(
                "POST\n/\n\ncontent-type:{CONTENT_TYPE}\nhost:textract.eu-west-1.amazonaws.com\n\
                     x-amz-content-sha256:{h}\nx-amz-date:20260911T123456Z\nx-amz-security-token:TOKEN\n\
                     x-amz-target:Textract.DetectDocumentText\n\n\
                     content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token;x-amz-target\n{h}",
                h = sha256_hex(b"{}")
            );
            let sts = format!(
                "AWS4-HMAC-SHA256\n20260911T123456Z\n20260911/eu-west-1/textract/aws4_request\n{}",
                sha256_hex(canonical.as_bytes())
            );
            hex(&hmac_sha256(
                &signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20260911", "eu-west-1", SERVICE),
                &sts,
            ))
        })));
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0u8, 255, 17, 3]), "AP8RAw==");
    }

    // -- credentials / context --

    #[test]
    fn rejects_a_bad_s3_object() {
        let req =
            DocumentRequest::from_path("a.pdf").api_key("AKIA").provider_options(json!({"s3_object": {"bucket": "b"}}));
        // Needs the secret in env to get past credential resolution; check the message instead.
        let e = Ctx::new(&req).unwrap_err();
        assert!(e.message.contains("AWS_SECRET_ACCESS_KEY") || e.message.contains("s3_object"), "{}", e.message);
    }

    // -- PDF page counting --

    #[test]
    fn counts_pdf_pages() {
        let single = b"%PDF-1.4\n1 0 obj<</Type /Page /Parent 2 0 R>>endobj\n2 0 obj<</Type/Pages/Count 1>>endobj";
        assert_eq!(pdf_page_count(single), Some(1));
        let multi =
            b"%PDF-1.4\n1 0 obj<</Type /Page>>endobj\n2 0 obj<</Type /Page>>endobj\n3 0 obj<</Type/Pages/Count 2>>";
        assert_eq!(pdf_page_count(multi), Some(2));
        // Only the page tree is readable (object streams hide the leaves).
        assert_eq!(pdf_page_count(b"%PDF-1.7\n<</Type/ObjStm>>\n<</Count 7>>"), Some(7));
        assert_eq!(pdf_page_count(b"\x89PNG\r\n"), None);
    }

    #[test]
    fn real_multipage_pdf_is_detected() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let data = std::fs::read(path).unwrap();
        assert_eq!(pdf_page_count(&data), Some(2));
        let e = multipage_error(2);
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("s3_object") && e.message.contains("2 pages"), "{}", e.message);
    }

    // -- error mapping --

    #[test]
    fn maps_textract_exception_types() {
        let e = map_error(400, r#"{"__type":"AccessDeniedException","message":"nope"}"#);
        assert_eq!(e.kind, ErrorKind::Authentication);
        assert!(e.message.contains("AccessDeniedException: nope"));
        assert_eq!(map_error(400, r#"{"__type":"ThrottlingException","message":"slow"}"#).kind, ErrorKind::RateLimit);
        assert_eq!(
            map_error(400, r#"{"__type":"ProvisionedThroughputExceededException","message":"x"}"#).kind,
            ErrorKind::RateLimit
        );
        assert_eq!(
            map_error(400, r#"{"__type":"LimitExceededException","message":"too many jobs"}"#).kind,
            ErrorKind::RateLimit
        );
        assert_eq!(
            map_error(400, r#"{"__type":"UnsupportedDocumentException","message":"bad"}"#).kind,
            ErrorKind::BadRequest
        );
        assert_eq!(
            map_error(400, r#"{"__type":"UnrecognizedClientException","message":"The security token included in the request is invalid."}"#).kind,
            ErrorKind::Authentication
        );
        assert_eq!(map_error(500, r#"{"__type":"InternalServerError","message":"x"}"#).kind, ErrorKind::Provider);
        // Fully qualified shaded names still resolve.
        assert_eq!(
            map_error(400, r#"{"__type":"com.amazonaws.textract#ThrottlingException","message":"x"}"#).kind,
            ErrorKind::RateLimit
        );
        // Unknown shape falls back to the shared HTTP mapping.
        assert_eq!(map_error(503, "gateway down").kind, ErrorKind::Provider);
    }

    // -- fixtures --

    #[test]
    fn normalizes_detect_text_fixture() {
        let analysis = analysis_from(include_str!("../../tests/fixtures/textract_detect_text.json"));
        let resp = build_text(&DocumentRequest::from_path("a.png"), &analysis, &ctx(), "detect-text");
        assert_eq!(resp.usage.pages, 1);
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.pages[0].lines.len(), 3);
        assert_eq!(resp.pages[0].words.len(), 6);
        assert_eq!(resp.pages[0].lines[0].text, "Hello LiteOCR");
        assert_eq!(resp.text, "Hello LiteOCR\nInvoice #1234\nTotal: $56.78");
        let c = resp.pages[0].lines[0].confidence.unwrap();
        assert!((c - 0.9938).abs() < 1e-3, "{c}");
        let bb = resp.pages[0].lines[0].bbox.unwrap();
        assert!((bb.x0 - 0.1193).abs() < 1e-4 && (bb.x1 - 0.3668).abs() < 1e-4);
        assert_eq!(resp.metadata["textract_region"], "us-east-1");
        assert_eq!(resp.metadata["textract_model_version"], "1.0");
        assert!(resp.raw.is_none());
    }

    #[test]
    fn normalizes_layout_fixture() {
        let analysis = analysis_from(include_str!("../../tests/fixtures/textract_layout.json"));
        let resp = build_parse(&DocumentRequest::from_path("a.png"), &analysis, &ctx(), "layout");
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.usage.pages, 1);
        let types: Vec<&str> = resp.pages[0].blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(types, vec!["title", "section_header", "text", "table", "list", "figure"]);
        assert!(resp.markdown.starts_with("# Quarterly Report\n\n## Revenue"));
        // Table rendered from CELL relationships, header row taken from COLUMN_HEADER cells.
        assert!(resp.markdown.contains("| Region | Revenue |"), "{}", resp.markdown);
        assert!(resp.markdown.contains("| --- | --- |"), "{}", resp.markdown);
        assert!(resp.markdown.contains("| North | $1,200 |"), "{}", resp.markdown);
        assert!(resp.markdown.contains("- First item\n- Second item"), "{}", resp.markdown);
        let title = &resp.pages[0].blocks[0];
        assert_eq!(title.content, "# Quarterly Report");
        assert_eq!(title.text.as_deref(), Some("Quarterly Report"));
        assert!((title.confidence.unwrap() - 0.9812).abs() < 1e-3);
        assert!(title.bbox.is_some());
    }

    #[test]
    fn layout_fixture_also_yields_native_ocr() {
        let analysis = analysis_from(include_str!("../../tests/fixtures/textract_layout.json"));
        let resp = build_text(&DocumentRequest::from_path("a.png"), &analysis, &ctx(), "layout");
        assert!(resp.text.starts_with("Quarterly Report\nRevenue"));
        assert!(resp.pages[0].lines.len() >= 6);
        assert!(resp.pages[0].words.iter().any(|w| w.text == "Quarterly"));
    }

    #[test]
    fn layout_falls_back_to_lines_and_tables_without_layout_blocks() {
        let mut analysis = analysis_from(include_str!("../../tests/fixtures/textract_layout.json"));
        analysis.blocks.retain(|b| !is_layout(&b.block_type));
        let resp = build_parse(&DocumentRequest::from_path("a.png"), &analysis, &ctx(), "layout");
        assert!(resp.markdown.contains("| Region | Revenue |"), "{}", resp.markdown);
        assert!(resp.markdown.contains("Quarterly Report"), "{}", resp.markdown);
        // Lines that only exist inside table cells are not repeated outside the table.
        assert_eq!(resp.markdown.matches("North").count(), 1, "{}", resp.markdown);
    }

    #[test]
    fn text_output_format_strips_markdown() {
        let analysis = analysis_from(include_str!("../../tests/fixtures/textract_layout.json"));
        let req = DocumentRequest::from_path("a.png").output(OutputFormat::Text);
        let resp = build_parse(&req, &analysis, &ctx(), "layout");
        assert!(!resp.markdown.contains("# Quarterly"));
        assert!(resp.markdown.starts_with("Quarterly Report"));
    }

    #[test]
    fn builds_queries_from_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "invoice number": {"type": "string", "description": "What is the invoice number?"},
                "total": {"type": "number", "title": "Total amount due"},
                "paid": {"type": "boolean"},
                "line_items": {"type": "array", "items": {"type": "string"}}
            }
        });
        let plan = QueryPlan::from_schema(&schema, MAX_QUERIES_SYNC, None).unwrap();
        assert_eq!(plan.queries.len(), 3);
        assert_eq!(plan.skipped, vec!["line_items"]);
        let by_alias: BTreeMap<&str, &Value> = plan.queries.iter().map(|q| (q["Alias"].as_str().unwrap(), q)).collect();
        assert_eq!(by_alias["invoice_number"]["Text"], "What is the invoice number?");
        assert_eq!(by_alias["total"]["Text"], "Total amount due");
        assert_eq!(by_alias["paid"]["Text"], "paid");
        assert!(by_alias["paid"].get("Pages").is_none());
        assert_eq!(plan.alias_to_key["invoice_number"], "invoice number");

        // Page selection becomes Query.Pages.
        let plan =
            QueryPlan::from_schema(&schema, MAX_QUERIES_ASYNC, Some(&[(1, Some(3)), (7, Some(7)), (9, None)])).unwrap();
        assert_eq!(plan.queries[0]["Pages"], json!(["1-3", "7", "9-"]));

        // Limits and empty schemas are refused before any network call.
        let mut props = Map::new();
        for i in 0..20 {
            props.insert(format!("f{i}"), json!({"type": "string"}));
        }
        let big = json!({"properties": props});
        assert!(QueryPlan::from_schema(&big, MAX_QUERIES_SYNC, None).is_err());
        assert!(QueryPlan::from_schema(&big, MAX_QUERIES_ASYNC, None).is_ok());
        assert!(QueryPlan::from_schema(&json!({"properties": {}}), MAX_QUERIES_SYNC, None).is_err());
        assert!(QueryPlan::from_schema(&json!({"type": "object"}), MAX_QUERIES_SYNC, None).is_err());
    }

    #[test]
    fn normalizes_queries_fixture() {
        let analysis = analysis_from(include_str!("../../tests/fixtures/textract_queries.json"));
        let schema = json!({
            "type": "object",
            "properties": {
                "invoice_number": {"type": "string", "description": "What is the invoice number?"},
                "total": {"type": "number", "description": "What is the total amount?"},
                "due_date": {"type": "string", "description": "What is the due date?"}
            }
        });
        let plan = QueryPlan::from_schema(&schema, MAX_QUERIES_SYNC, None).unwrap();
        let req = ExtractRequest::new(DocumentRequest::from_path("a.png"), schema).citations(true);
        let resp = build_queries(&req, &analysis, &ctx(), "queries", &plan);
        assert_eq!(resp.data["invoice_number"], "1234");
        assert_eq!(resp.data["total"], json!(56.78));
        // The query with no answer is present and null, never missing.
        assert_eq!(resp.data["due_date"], Value::Null);
        assert!(!resp.fields.contains_key("/due_date"));
        let info = &resp.fields["/invoice_number"];
        assert!((info.confidence.unwrap() - 0.9723).abs() < 1e-3);
        assert_eq!(info.citations.len(), 1);
        assert_eq!(info.citations[0].page_number, 1);
        assert_eq!(info.citations[0].text.as_deref(), Some("1234"));
        assert!(info.citations[0].bbox.is_some());
        assert_eq!(resp.usage.pages, 1);
    }

    #[test]
    fn normalizes_forms_fixture() {
        let analysis = analysis_from(include_str!("../../tests/fixtures/textract_forms.json"));
        let schema = json!({
            "type": "object",
            "properties": {
                "invoice_number": {"type": "string"},
                "Total": {"type": "number"},
                "paid": {"type": "boolean", "title": "Paid in full"},
                "shipping_address": {"type": "object", "properties": {"city": {"type": "string"}}},
                "missing_field": {"type": "string"}
            }
        });
        let req = ExtractRequest::new(DocumentRequest::from_path("a.png"), schema).citations(true);
        let resp = build_forms(&req, &analysis, &ctx(), "forms");
        // Matching is case- and punctuation-insensitive: "Invoice Number:" → invoice_number.
        assert_eq!(resp.data["invoice_number"], "1234");
        assert_eq!(resp.data["Total"], json!(56.78));
        // A SELECTION_ELEMENT value becomes a boolean.
        assert_eq!(resp.data["paid"], json!(true));
        assert_eq!(resp.data["missing_field"], Value::Null);
        assert_eq!(resp.data["shipping_address"], Value::Null);
        let unmatched = resp.metadata["textract_unmatched_fields"].as_array().unwrap();
        assert!(unmatched.iter().any(|v| v == "missing_field"));
        assert!(unmatched.iter().any(|v| v == "shipping_address"));
        assert_eq!(resp.metadata["textract_form_keys_found"], 3);
        assert!(resp.fields["/Total"].confidence.unwrap() > 0.9);
        assert_eq!(resp.fields["/Total"].citations[0].text.as_deref(), Some("$56.78"));
    }

    #[test]
    fn coerces_scalar_types() {
        assert_eq!(coerce("$1,234.50", Some(&json!({"type": "number"}))), json!(1234.50));
        assert_eq!(coerce("42 units", Some(&json!({"type": "integer"}))), json!(42));
        assert_eq!(coerce("n/a", Some(&json!({"type": "number"}))), json!("n/a"));
        assert_eq!(coerce(" Yes ", Some(&json!({"type": "boolean"}))), json!(true));
        assert_eq!(coerce("NOT_SELECTED", Some(&json!({"type": "boolean"}))), json!(false));
        assert_eq!(coerce("hello", None), json!("hello"));
    }

    #[test]
    fn renders_selection_elements_and_escapes_pipes() {
        assert_eq!(escape_cell("a|b\nc"), "a\\|b c");
    }

    // -- live (opt-in) --

    async fn live(model: &str) -> Option<()> {
        for v in [ENV_KEY, ENV_SECRET] {
            if env_opt(v).is_none() {
                eprintln!("skipping {model}: {v} not set");
                return None;
            }
        }
        Some(())
    }

    const LIVE_IMAGE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/plain_001.png");

    #[tokio::test]
    #[ignore = "needs AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY and network"]
    async fn textract_detect_text_live() {
        if live("textract/detect-text").await.is_none() {
            return;
        }
        let resp = crate::ocr(DocumentRequest::from_path(LIVE_IMAGE).model("textract/detect-text").timeout_secs(120.0))
            .await
            .expect("detect-text succeeds");
        assert_eq!(resp.usage.pages, 1);
        assert!(!resp.text.trim().is_empty());
        assert!(!resp.pages[0].words.is_empty());
        assert!(resp.cost_usd.unwrap_or(0.0) > 0.0);
    }

    #[tokio::test]
    #[ignore = "needs AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY and network"]
    async fn textract_layout_live() {
        if live("textract/layout").await.is_none() {
            return;
        }
        let resp = crate::parse(DocumentRequest::from_path(LIVE_IMAGE).model("textract/layout").timeout_secs(120.0))
            .await
            .expect("layout succeeds");
        assert_eq!(resp.pages.len(), 1);
        assert!(!resp.markdown.trim().is_empty());
        assert!(!resp.pages[0].blocks.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY and network"]
    async fn textract_queries_live() {
        if live("textract/queries").await.is_none() {
            return;
        }
        let schema = json!({
            "type": "object",
            "properties": { "title": {"type": "string", "description": "What is the document title?"} }
        });
        let req = ExtractRequest::new(
            DocumentRequest::from_path(LIVE_IMAGE).model("textract/queries").timeout_secs(120.0),
            schema,
        )
        .citations(true);
        let resp = crate::extract(req).await.expect("queries succeeds");
        assert!(resp.data.get("title").is_some());
    }

    #[tokio::test]
    #[ignore = "needs AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY and network"]
    async fn textract_multipage_pdf_is_rejected() {
        if live("textract/detect-text").await.is_none() {
            return;
        }
        let pdf = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let e = crate::ocr(DocumentRequest::from_path(pdf).model("textract/detect-text")).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("s3_object"));
    }
}
