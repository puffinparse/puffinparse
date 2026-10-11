//! The unified request / response model. Every provider is normalised to these types.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the document comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocumentInput {
    /// A local file path.
    Path { path: PathBuf },
    /// In-memory bytes. `filename` is required so providers can infer the type.
    Bytes {
        #[serde(with = "base64_bytes")]
        data: bytes::Bytes,
        filename: String,
    },
    /// A publicly reachable http(s) URL.
    Url { url: String },
}

impl DocumentInput {
    /// Best-effort filename for uploads.
    pub fn filename(&self) -> String {
        match self {
            DocumentInput::Path { path } => {
                path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into())
            }
            DocumentInput::Bytes { filename, .. } => filename.clone(),
            DocumentInput::Url { url } => url
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.split('?').next().unwrap_or(s).to_string())
                .unwrap_or_else(|| "document".into()),
        }
    }

    /// MIME type guessed from the filename extension.
    pub fn mime_type(&self) -> String {
        mime_guess::from_path(self.filename()).first_or_octet_stream().essence_str().to_string()
    }

    /// Short description used in logs / error messages (never includes bytes).
    pub fn describe(&self) -> String {
        match self {
            DocumentInput::Path { path } => path.display().to_string(),
            DocumentInput::Bytes { data, filename } => format!("{filename} ({} bytes)", data.len()),
            DocumentInput::Url { url } => redact_url(url),
        }
    }
}

/// A URL safe to log: credentials, the query string and the fragment are dropped (presigned
/// links carry their signature there), with a marker when something was removed.
fn redact_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        // Not a URL we can take apart: keep the part before any query, fragment or `@`.
        let cut = raw.find(['?', '#', '@']).unwrap_or(raw.len());
        return if cut < raw.len() { format!("{}<redacted>", &raw[..cut]) } else { raw.to_string() };
    };
    let had_secret = !url.username().is_empty() || url.password().is_some();
    let had_query = url.query().is_some() || url.fragment().is_some();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    let mut out = url.to_string();
    if had_secret {
        out = out.replacen("://", "://<redacted>@", 1);
    }
    if had_query {
        out.push_str("?<redacted>");
    }
    out
}

/// Free-form maps in `Debug` output: the keys only, since a value may be a token.
fn keys_only<'a>(keys: impl Iterator<Item = &'a String>) -> BTreeMap<&'a str, &'static str> {
    keys.map(|k| (k.as_str(), "<redacted>")).collect()
}

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
        }
        out
    }

    fn decode(s: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        let mut buf = 0u32;
        let mut bits = 0;
        for c in s.bytes() {
            if c == b'=' {
                break;
            }
            let v = ALPHABET.iter().position(|&a| a == c).ok_or_else(|| format!("invalid base64 byte {c}"))? as u32;
            buf = (buf << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(b: &bytes::Bytes, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<bytes::Bytes, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map(bytes::Bytes::from).map_err(serde::de::Error::custom)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn roundtrip() {
            for input in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0u8, 255, 17, 3]] {
                assert_eq!(super::decode(&super::encode(input)).unwrap(), input);
            }
            assert_eq!(super::encode(b"foobar"), "Zm9vYmFy");
        }
    }
}

/// Preferred textual representation for block `content`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Markdown,
    Text,
}

/// A unified OCR request.
///
/// `Debug` is written by hand: it never prints `api_key` or `webhook_url` (which can carry a
/// shared secret such as a gateway `?token=`), and shows document bytes as their length only.
#[derive(Clone, Serialize, Deserialize)]
pub struct DocumentRequest {
    pub input: DocumentInput,
    /// `"<provider>/<model>"`, e.g. `"reducto/standard"`. `"reducto"` selects the default model.
    pub model: String,
    /// 1-based page selection, e.g. `"1-3,7"`. Forwarded best-effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<String>,
    /// Language hint (BCP-47 / ISO 639-1), forwarded when the provider supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub output: OutputFormat,
    /// Render the response in a provider's *native* JSON shape instead of the unified one:
    /// `"reducto"`, `"extend"`, `"llamaparse"` (aliases `"llama"`, `"llama_parse"`), or
    /// `"puffinparse"`/`None` for the unified shape. Independent of [`OutputFormat`], which selects
    /// markdown vs plain text inside block `content`.
    ///
    /// The core never changes the type it returns: validate the string with
    /// [`DocumentRequest::validate_output_format`] when building the request, then call
    /// [`ParseResponse::to_format`] on the result. See `docs/COMPAT.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// Provider-specific options merged verbatim into the provider request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<serde_json::Value>,
    /// Attach the provider's raw payload to `ParseResponse::raw`.
    #[serde(default)]
    pub include_raw: bool,
    /// Whole-call deadline in seconds (upload + polling + result download).
    #[serde(default = "default_timeout")]
    pub timeout_secs: f64,
    /// Retries on 429 / 5xx / network errors (exponential backoff with jitter).
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Override the API key (otherwise read from the provider's env var).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override the provider base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// A URL the provider should POST to when an asynchronous job finishes. Only used by
    /// [`crate::submit_parse`]; mapped to each provider's native webhook setting (Reducto
    /// `async.webhook`, LlamaParse `webhook_url`). Providers without per-job webhooks reject it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    /// Free-form metadata echoed back in the response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl std::fmt::Debug for DocumentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("DocumentRequest")
            .field("input", &self.input.describe())
            .field("model", &self.model)
            .field("pages", &self.pages)
            .field("language", &self.language)
            .field("output", &self.output)
            .field("output_format", &self.output_format)
            .field(
                "provider_options",
                &self.provider_options.as_ref().map(|v| match v {
                    serde_json::Value::Object(m) => format!("{:?}", keys_only(m.keys())),
                    _ => "<redacted>".to_string(),
                }),
            )
            .field("include_raw", &self.include_raw)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_retries", &self.max_retries)
            .field("api_key", &redacted(&self.api_key))
            .field("base_url", &self.base_url)
            .field("webhook_url", &redacted(&self.webhook_url))
            .field("metadata", &keys_only(self.metadata.keys()))
            .finish()
    }
}

fn default_timeout() -> f64 {
    300.0
}

fn default_retries() -> u32 {
    2
}

impl DocumentRequest {
    pub fn new(input: DocumentInput) -> Self {
        Self {
            input,
            model: "reducto".to_string(),
            pages: None,
            language: None,
            output: OutputFormat::Markdown,
            output_format: None,
            provider_options: None,
            include_raw: false,
            timeout_secs: default_timeout(),
            max_retries: default_retries(),
            api_key: None,
            base_url: None,
            webhook_url: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self::new(DocumentInput::Path { path: path.as_ref().to_path_buf() })
    }

    pub fn from_bytes(data: impl Into<bytes::Bytes>, filename: impl Into<String>) -> Self {
        Self::new(DocumentInput::Bytes { data: data.into(), filename: filename.into() })
    }

    pub fn from_url(url: impl Into<String>) -> Self {
        Self::new(DocumentInput::Url { url: url.into() })
    }

    /// Build from a string that is either a URL or a local path.
    pub fn from_str_input(s: &str) -> Self {
        if s.starts_with("http://") || s.starts_with("https://") {
            Self::from_url(s)
        } else {
            Self::from_path(s)
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn pages(mut self, pages: impl Into<String>) -> Self {
        self.pages = Some(pages.into());
        self
    }

    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    pub fn output(mut self, output: OutputFormat) -> Self {
        self.output = output;
        self
    }

    /// Ask for the response in a provider's native shape (`"reducto"`, `"extend"`, `"llamaparse"`).
    pub fn output_format(mut self, format: impl Into<String>) -> Self {
        self.output_format = Some(format.into());
        self
    }

    /// Fail fast on an unknown `output_format` at request-build time, before any provider call.
    /// `None` (the unified shape) is always valid.
    pub fn validate_output_format(&self) -> crate::error::Result<()> {
        match &self.output_format {
            None => Ok(()),
            Some(s) => s.parse::<crate::compat::Format>().map(|_| ()),
        }
    }

    /// The validated [`crate::compat::Format`] this request asks for, defaulting to the unified shape.
    pub fn compat_format(&self) -> crate::error::Result<crate::compat::Format> {
        match &self.output_format {
            None => Ok(crate::compat::Format::Puffinparse),
            Some(s) => s.parse(),
        }
    }

    pub fn provider_options(mut self, options: serde_json::Value) -> Self {
        self.provider_options = Some(options);
        self
    }

    pub fn include_raw(mut self, include: bool) -> Self {
        self.include_raw = include;
        self
    }

    pub fn timeout_secs(mut self, secs: f64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Ask the provider to POST to `url` when a job started with [`crate::submit_parse`] finishes.
    pub fn webhook_url(mut self, url: impl Into<String>) -> Self {
        self.webhook_url = Some(url.into());
        self
    }

    /// Look up a provider option by key, if `provider_options` is an object.
    pub fn option(&self, key: &str) -> Option<&serde_json::Value> {
        self.provider_options.as_ref().and_then(|v| v.get(key))
    }
}

/// Semantic type of a block, mapped from each provider's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockType {
    Text,
    Title,
    SectionHeader,
    List,
    Table,
    Figure,
    Header,
    Footer,
    Footnote,
    Caption,
    Formula,
    Other,
}

impl BlockType {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockType::Text => "text",
            BlockType::Title => "title",
            BlockType::SectionHeader => "section_header",
            BlockType::List => "list",
            BlockType::Table => "table",
            BlockType::Figure => "figure",
            BlockType::Header => "header",
            BlockType::Footer => "footer",
            BlockType::Footnote => "footnote",
            BlockType::Caption => "caption",
            BlockType::Formula => "formula",
            BlockType::Other => "other",
        }
    }
}

/// Normalised bounding box: coordinates in `0..=1` relative to page size, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BBox {
    /// Build from absolute `(x, y, w, h)` plus page dimensions; returns `None` if dims are unusable.
    pub fn from_xywh(x: f64, y: f64, w: f64, h: f64, page_w: f64, page_h: f64) -> Option<Self> {
        if page_w <= 0.0 || page_h <= 0.0 {
            return None;
        }
        Some(Self {
            x0: (x / page_w).clamp(0.0, 1.0),
            y0: (y / page_h).clamp(0.0, 1.0),
            x1: ((x + w) / page_w).clamp(0.0, 1.0),
            y1: ((y + h) / page_h).clamp(0.0, 1.0),
        })
    }

    /// Build from already-normalised `(left, top, width, height)`.
    pub fn from_normalized_ltwh(left: f64, top: f64, width: f64, height: f64) -> Self {
        Self {
            x0: left.clamp(0.0, 1.0),
            y0: top.clamp(0.0, 1.0),
            x1: (left + width).clamp(0.0, 1.0),
            y1: (top + height).clamp(0.0, 1.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    #[serde(rename = "type")]
    pub block_type: BlockType,
    /// Markdown (or text, per `OutputFormat`) content of the block.
    pub content: String,
    /// Plain-text variant when the provider supplies one separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// 1-based page number.
    pub page_number: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// 1-based.
    pub page_number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    pub markdown: String,
    pub text: String,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Pages processed / billed.
    pub pages: u32,
    /// Provider-native credit units, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<f64>,
    /// Dollar cost when the provider reports it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParseResponse {
    /// PuffinParse-generated id (UUID v4).
    pub id: String,
    pub provider: String,
    /// Fully-qualified model, e.g. `"reducto/standard"`.
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_job_id: Option<String>,
    pub pages: Vec<Page>,
    /// Whole-document markdown: pages joined by a blank line.
    pub markdown: String,
    pub text: String,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    /// RFC 3339 timestamp.
    pub created_at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl ParseResponse {
    /// Construct a response from pages, deriving document-level markdown/text.
    pub fn from_pages(provider: &str, model: &str, mut pages: Vec<Page>, usage: Usage) -> Self {
        pages.sort_by_key(|p| p.page_number);
        let markdown = join_pages(pages.iter().map(|p| p.markdown.as_str()));
        let text = join_pages(pages.iter().map(|p| p.text.as_str()));
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            provider_job_id: None,
            pages,
            markdown,
            text,
            usage,
            cost_usd: None,
            latency_ms: 0,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            metadata: BTreeMap::new(),
            raw: None,
        }
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Render this response in a provider's native JSON shape.
    ///
    /// `format` is one of `"puffinparse"`, `"reducto"`, `"extend"`, `"llamaparse"` (aliases `"llama"`,
    /// `"llama_parse"`); unknown values are an [`crate::ErrorKind::Input`] error. This is what the
    /// SDK and CLI call when the caller set [`DocumentRequest::output_format`].
    /// See `docs/COMPAT.md` for the exact guarantees.
    pub fn to_format(&self, format: &str) -> crate::error::Result<serde_json::Value> {
        Ok(crate::compat::render_parse(self, format.parse()?))
    }
}

/// Join page strings with a blank line, skipping empty pages and trimming edges.
pub fn join_pages<'a>(pages: impl Iterator<Item = &'a str>) -> String {
    pages.map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// Group blocks into pages by their `page_number`, producing markdown/text per page.
pub fn pages_from_blocks(blocks: Vec<Block>, page_dims: &BTreeMap<u32, (f64, f64)>) -> Vec<Page> {
    let mut by_page: BTreeMap<u32, Vec<Block>> = BTreeMap::new();
    for b in blocks {
        by_page.entry(b.page_number).or_default().push(b);
    }
    by_page
        .into_iter()
        .map(|(page_number, blocks)| {
            let markdown = join_pages(blocks.iter().map(|b| b.content.as_str()));
            let texts: Vec<String> =
                blocks.iter().map(|b| b.text.clone().unwrap_or_else(|| markdown_to_text(&b.content))).collect();
            let text = join_pages(texts.iter().map(String::as_str));
            let (width, height) = page_dims.get(&page_number).map(|&(w, h)| (Some(w), Some(h))).unwrap_or((None, None));
            Page { page_number, width, height, markdown, text, blocks }
        })
        .collect()
}

/// Inline HTML elements: their tags sit inside a word (`9<sup>th</sup>`, `<i>Nature</i>,`), so
/// removing them must not split the word. Every other tag (`<br>`, `<p>`, `<td>`, …) separates text.
const INLINE_TAGS: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "del", "dfn", "em", "font", "i", "ins", "kbd", "mark", "q",
    "s", "samp", "small", "span", "strike", "strong", "sub", "sup", "time", "tt", "u", "var",
];

/// Remove HTML tags that some providers embed in markdown: inline tags (`<b>`, `</i>`, `<sup>`)
/// vanish, any other tag (`<br/>`, `<p>`) becomes a space.
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // Only treat it as a tag if it looks like one: `<tag`, `</tag`, `<tag/>`.
            let mut probe = chars.clone();
            let looks_like_tag = matches!(probe.next(), Some(n) if n.is_ascii_alphabetic() || n == '/');
            if looks_like_tag {
                let mut closed = false;
                let mut inner = String::new();
                for n in chars.by_ref() {
                    if n == '>' {
                        closed = true;
                        break;
                    }
                    inner.push(n);
                }
                if closed {
                    let name: String = inner
                        .trim_start_matches('/')
                        .chars()
                        .take_while(char::is_ascii_alphanumeric)
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if !INLINE_TAGS.contains(&name.as_str()) {
                        out.push(' ');
                    }
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Drop single `*` / `_` emphasis delimiters (`*italic*`, `_italic_`). A delimiter is a marker
/// with text on one side and a space, punctuation or the line edge on the other; a lone ` * `
/// (multiplication) and intraword markers (`snake_case`, `a*b`) are kept, as in CommonMark.
fn strip_single_emphasis(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let open = |c: Option<&char>| match c {
        None => true,
        Some(&c) => c.is_whitespace() || (c.is_ascii_punctuation() && c != '*' && c != '_'),
    };
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '*' || c == '_' {
            let before = open(if i == 0 { None } else { chars.get(i - 1) });
            let after = open(chars.get(i + 1));
            if before != after {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Very small markdown → plain text conversion, used when a provider only gives markdown.
pub fn markdown_to_text(md: &str) -> String {
    let md = strip_html_tags(md);
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        let l = line.trim_end();
        let trimmed = l.trim_start();
        // Skip table separator rows like |---|---|
        if trimmed.starts_with('|') && trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
            continue;
        }
        let mut s = trimmed.trim_start_matches('#').trim_start().to_string();
        if trimmed.starts_with('#') && s.is_empty() {
            continue;
        }
        for prefix in ["- ", "* ", "+ ", "> "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.to_string();
                break;
            }
        }
        if s.starts_with('|') {
            s = s.trim_matches('|').split('|').map(str::trim).collect::<Vec<_>>().join(" ");
        }
        let s = strip_single_emphasis(&s.replace("**", "").replace("__", "").replace('`', ""));
        out.push_str(s.trim());
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_secrets() {
        let mut r = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"%PDF-1.4 body"), "a.pdf")
            .api_key("sk-live-SECRET-123");
        r.webhook_url = Some("https://gw.example/v1/webhooks/reducto?token=HOOK-SECRET".into());
        let dbg = format!("{r:?} {r:#?}");
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(!dbg.contains("PDF-1.4 body"), "{dbg}");
        assert!(dbg.contains("<redacted>") && dbg.contains("a.pdf (13 bytes)"), "{dbg}");
        let x = ExtractRequest::new(r, serde_json::json!({"type": "object"}));
        assert!(!format!("{x:?}").contains("SECRET"));
    }

    #[test]
    fn debug_and_describe_hide_url_secrets_and_free_form_values() {
        let mut r = DocumentRequest::from_str_input(
            "https://user:PASS-SECRET@bucket.s3.example/a/doc.pdf?X-Amz-Signature=SIG-SECRET#frag-SECRET",
        );
        r.provider_options = Some(serde_json::json!({"token": "OPT-SECRET", "mode": "fast"}));
        r.metadata.insert("auth".into(), serde_json::json!("META-SECRET"));
        let dbg = format!("{r:?} {r:#?} {}", r.input.describe());
        assert!(!dbg.contains("SECRET"), "{dbg}");
        assert!(dbg.contains("https://<redacted>@bucket.s3.example/a/doc.pdf?<redacted>"), "{dbg}");
        assert!(dbg.contains("token") && dbg.contains("mode") && dbg.contains("auth"), "keys stay visible: {dbg}");
        // A plain URL is unchanged; something that does not parse is cut before any secret.
        assert_eq!(DocumentInput::Url { url: "https://a.example/b.pdf".into() }.describe(), "https://a.example/b.pdf");
        assert_eq!(DocumentInput::Url { url: "not a url?k=SECRET".into() }.describe(), "not a url<redacted>");
        let r = DocumentRequest { provider_options: Some(serde_json::json!("RAW-SECRET")), ..r };
        assert!(!format!("{r:?}").contains("SECRET"));
    }

    #[test]
    fn input_filename_and_mime() {
        let p = DocumentInput::Path { path: "/tmp/a/invoice.PDF".into() };
        assert_eq!(p.filename(), "invoice.PDF");
        assert_eq!(p.mime_type(), "application/pdf");
        let u = DocumentInput::Url { url: "https://x.com/y/z.png?token=1".into() };
        assert_eq!(u.filename(), "z.png");
        assert_eq!(u.mime_type(), "image/png");
        let b = DocumentInput::Bytes { data: bytes::Bytes::from_static(b"x"), filename: "a.bin".into() };
        assert_eq!(b.mime_type(), "application/octet-stream");
    }

    #[test]
    fn request_from_str_detects_url() {
        assert!(matches!(DocumentRequest::from_str_input("https://a/b.pdf").input, DocumentInput::Url { .. }));
        assert!(matches!(DocumentRequest::from_str_input("b.pdf").input, DocumentInput::Path { .. }));
    }

    #[test]
    fn groups_blocks_into_pages() {
        let mk = |p: u32, c: &str| Block {
            block_type: BlockType::Text,
            content: c.into(),
            text: None,
            bbox: None,
            confidence: None,
            page_number: p,
        };
        let pages = pages_from_blocks(vec![mk(2, "b"), mk(1, "a"), mk(1, "a2")], &BTreeMap::new());
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].page_number, 1);
        assert_eq!(pages[0].markdown, "a\n\na2");
        assert_eq!(pages[1].markdown, "b");
        let resp = ParseResponse::from_pages("p", "p/m", pages, Usage::default());
        assert_eq!(resp.markdown, "a\n\na2\n\nb");
    }

    #[test]
    fn markdown_to_text_strips_syntax() {
        let md = "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- **bold** item";
        assert_eq!(markdown_to_text(md), "Title\n\na b\n1 2\n\nbold item");
    }

    #[test]
    fn strips_inline_html() {
        assert_eq!(strip_html_tags("<b>Bold</b> and <i>it</i><br/>x"), "Bold and it x");
        assert_eq!(strip_html_tags("9<sup>th</sup> and H<sub>2</sub>O<p>next"), "9th and H2O next");
        assert_eq!(strip_html_tags("a < b and c > d"), "a < b and c > d");
        assert_eq!(markdown_to_text("# <b>Title</b>\n\n| <b>ID</b> | x |\n|-|-|\n| 1 | 2 |"), "Title\n\nID x\n1 2");
    }

    #[test]
    fn markdown_to_text_strips_single_emphasis() {
        assert_eq!(markdown_to_text("An *italic* and _also_ word, (*x*)."), "An italic and also word, (x).");
        assert_eq!(markdown_to_text("***both*** here"), "both here");
        assert_eq!(markdown_to_text("keep snake_case, a*b and 2 * 3"), "keep snake_case, a*b and 2 * 3");
        assert_eq!(markdown_to_text("* bullet with *em*"), "bullet with em");
    }

    #[test]
    fn bbox_normalises() {
        let b = BBox::from_xywh(10.0, 20.0, 30.0, 40.0, 100.0, 200.0).unwrap();
        assert!((b.x0 - 0.1).abs() < 1e-9 && (b.y1 - 0.3).abs() < 1e-9);
        assert!(BBox::from_xywh(1.0, 1.0, 1.0, 1.0, 0.0, 0.0).is_none());
    }
}

// ---- modes ---------------------------------------------------------------------------------------

/// What kind of work a call asks a provider to do. Providers can only be swapped within a mode.
///
/// - `parse`: layout-aware parsing → markdown + typed blocks with boxes ([`ParseResponse`]).
/// - `ocr`: plain text recognition → text + words/lines with boxes ([`TextResponse`]).
/// - `extract`: schema-driven structured extraction → JSON + citations ([`ExtractResponse`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Parse,
    Ocr,
    Extract,
}

impl Mode {
    pub const ALL: &'static [Mode] = &[Mode::Parse, Mode::Ocr, Mode::Extract];

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Parse => "parse",
            Mode::Ocr => "ocr",
            Mode::Extract => "extract",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "parse" => Ok(Mode::Parse),
            "ocr" | "text" => Ok(Mode::Ocr),
            "extract" | "extraction" => Ok(Mode::Extract),
            other => Err(crate::error::Error::input(format!("unknown mode '{other}' (parse | ocr | extract)"))),
        }
    }
}

// ---- ocr mode ------------------------------------------------------------------------------------

/// A recognised word with its box and confidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// A recognised line of text (a run of words on one baseline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPage {
    /// 1-based.
    pub page_number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
    /// Plain text in reading order, lines separated by `\n`.
    pub text: String,
    #[serde(default)]
    pub lines: Vec<Line>,
    #[serde(default)]
    pub words: Vec<Word>,
}

/// Result of an `ocr`-mode call: plain text with word/line geometry, no layout semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextResponse {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_job_id: Option<String>,
    pub pages: Vec<TextPage>,
    /// Whole-document text, pages joined by a blank line.
    pub text: String,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl TextResponse {
    pub fn from_pages(provider: &str, model: &str, mut pages: Vec<TextPage>, usage: Usage) -> Self {
        pages.sort_by_key(|p| p.page_number);
        let text = join_pages(pages.iter().map(|p| p.text.as_str()));
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            provider_job_id: None,
            pages,
            text,
            usage,
            cost_usd: None,
            latency_ms: 0,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            metadata: BTreeMap::new(),
            raw: None,
        }
    }

    /// Derive a plain-text result from a layout parse (for providers without a native OCR endpoint).
    /// Lines come from block text split on newlines; words carry no geometry.
    pub fn from_parse(resp: &ParseResponse) -> Self {
        let pages = resp
            .pages
            .iter()
            .map(|p| {
                let lines: Vec<Line> = p
                    .blocks
                    .iter()
                    .flat_map(|b| {
                        let text = b.text.clone().unwrap_or_else(|| markdown_to_text(&b.content));
                        let bbox = b.bbox;
                        let confidence = b.confidence;
                        text.lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .map(|l| Line { text: l.to_string(), bbox, confidence })
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let words: Vec<Word> = lines
                    .iter()
                    .flat_map(|l| {
                        l.text
                            .split_whitespace()
                            .map(|w| Word { text: w.to_string(), bbox: None, confidence: l.confidence })
                            .collect::<Vec<_>>()
                    })
                    .collect();
                TextPage {
                    page_number: p.page_number,
                    width: p.width,
                    height: p.height,
                    text: p.text.clone(),
                    lines,
                    words,
                }
            })
            .collect();
        let mut out = Self::from_pages(&resp.provider, &resp.model, pages, resp.usage.clone());
        out.provider_job_id = resp.provider_job_id.clone();
        out.metadata = resp.metadata.clone();
        out.metadata.insert("puffinparse_derived_from".into(), serde_json::json!("parse"));
        out.raw = resp.raw.clone();
        out
    }
}

// ---- extract mode --------------------------------------------------------------------------------

/// A schema-driven extraction request: a document plus a JSON Schema describing the fields wanted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    /// Document, model, timeouts, provider options — same as a parse request.
    #[serde(flatten)]
    pub document: DocumentRequest,
    /// JSON Schema (draft 2020-12 subset) for the output object.
    pub schema: serde_json::Value,
    /// Optional natural-language guidance forwarded to providers that accept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Ask for per-field citations (page + box + source text) when the provider supports them.
    #[serde(default)]
    pub citations: bool,
}

impl ExtractRequest {
    pub fn new(document: DocumentRequest, schema: serde_json::Value) -> Self {
        Self { document, schema, instructions: None, citations: false }
    }

    pub fn instructions(mut self, s: impl Into<String>) -> Self {
        self.instructions = Some(s.into());
        self
    }

    pub fn citations(mut self, on: bool) -> Self {
        self.citations = on;
        self
    }
}

/// Where an extracted value came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// 1-based page.
    pub page_number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
    /// Source text the value was read from, if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Per-field metadata (confidence, citations) keyed by JSON pointer (`/invoice/total`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FieldInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<Citation>,
}

/// Result of an `extract`-mode call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractResponse {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_job_id: Option<String>,
    /// The extracted object, shaped by the request schema.
    pub data: serde_json::Value,
    /// Per-field confidence and citations keyed by JSON pointer into `data`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, FieldInfo>,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl ExtractResponse {
    pub fn new(provider: &str, model: &str, data: serde_json::Value, usage: Usage) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            provider_job_id: None,
            data,
            fields: BTreeMap::new(),
            usage,
            cost_usd: None,
            latency_ms: 0,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            metadata: BTreeMap::new(),
            raw: None,
        }
    }

    /// Render this response in a provider's native extract JSON shape (best effort — see
    /// `docs/COMPAT.md`). Accepts the same names as [`ParseResponse::to_format`].
    pub fn to_format(&self, format: &str) -> crate::error::Result<serde_json::Value> {
        Ok(crate::compat::render_extract(self, format.parse()?))
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn mode_parses() {
        assert_eq!("parse".parse::<Mode>().unwrap(), Mode::Parse);
        assert_eq!("OCR".parse::<Mode>().unwrap(), Mode::Ocr);
        assert_eq!("extract".parse::<Mode>().unwrap(), Mode::Extract);
        assert!("nope".parse::<Mode>().is_err());
    }

    #[test]
    fn text_from_parse_derives_lines_and_words() {
        let block = Block {
            block_type: BlockType::Text,
            content: "Hello **world**\nSecond line".into(),
            text: None,
            bbox: Some(BBox { x0: 0.1, y0: 0.1, x1: 0.5, y1: 0.2 }),
            confidence: Some(0.9),
            page_number: 1,
        };
        let pages = pages_from_blocks(vec![block], &BTreeMap::new());
        let parse = ParseResponse::from_pages("p", "p/m", pages, Usage { pages: 1, ..Default::default() });
        let text = TextResponse::from_parse(&parse);
        assert_eq!(text.pages[0].lines.len(), 2);
        assert_eq!(text.pages[0].lines[0].text, "Hello world");
        assert_eq!(text.pages[0].words.len(), 4);
        assert_eq!(text.text, "Hello world\nSecond line");
        assert_eq!(text.metadata["puffinparse_derived_from"], "parse");
    }
}
