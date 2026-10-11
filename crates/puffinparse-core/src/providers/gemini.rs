//! Google Gemini (Generative Language API) — vision-LLM document parsing.
//!
//! One call, no polling: `POST {base}/v1beta/models/{model}:generateContent` with the document as
//! an `inline_data` part (base64) plus a prompt, and `generationConfig.response_schema` pinning the
//! output shape so pages come back exactly. Documents larger than ~14 MB are pushed through the
//! resumable Files API first (`POST {base}/upload/v1beta/files`) and referenced as `file_data`.
//!
//! Gemini is a language model, not a layout engine: it returns text, never geometry. Blocks are
//! therefore one `text` block per page with `bbox: None`, and `ocr` mode is derived from `parse`.

use super::vlm::{base64_encode, pdf_page_count, sniff_mime, strip_code_fence};
use crate::error::{Error, ErrorKind, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
#[cfg(test)]
use crate::types::BlockType;
use crate::types::{
    DocumentInput, DocumentRequest, ExtractRequest, ExtractResponse, OutputFormat, Page, ParseResponse, Usage,
};
use crate::util::deep_merge;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::Duration;

pub const NAME: &str = "gemini";
const ENV_KEY: &str = "GEMINI_API_KEY";
const ENV_BASE: &str = "GEMINI_BASE_URL";
const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";
/// Path version. `v1beta` is what the Files API and the newest models are served under.
const API_VERSION: &str = "v1beta";
/// A `generateContent` request is capped at ~20 MB and `inline_data` is base64 (+33 %), so anything
/// above this goes through the Files API instead.
const INLINE_MAX_BYTES: usize = 14 * 1024 * 1024;
/// Refuse to buffer absurd remote documents (the Files API tops out at 2 GB, Gemini docs at 50 MB).
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Default, Clone, Copy)]
pub struct Gemini;

#[async_trait]
impl Provider for Gemini {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn parse(&self, request: &DocumentRequest, model: &str) -> Result<ParseResponse> {
        let call = Call::new(request, model)?;
        let source = call.prepare(request).await?;
        let prompt = parse_prompt(request, &source);
        let body = call.body(&source, prompt, pages_schema(), request);
        let wire = call.generate(body).await?;
        let raw = if request.include_raw { Some(serde_json::to_value(&wire)?) } else { None };
        let mut resp = normalize_parse(&wire, request.output, model, &call.api_model, source.pdf_page_count())?;
        if request.pages.is_some() && !source.is_pdf() {
            resp.metadata.insert("gemini_pages_ignored".into(), json!(true));
        }
        resp.raw = raw;
        Ok(resp)
    }

    async fn extract(&self, request: &ExtractRequest, model: &str) -> Result<ExtractResponse> {
        let doc = &request.document;
        let call = Call::new(doc, model)?;
        let source = call.prepare(doc).await?;
        let schema = sanitize_schema(&request.schema);
        let prompt = extract_prompt(request, &schema, &source);
        let body = call.body(&source, prompt, schema, doc);
        let wire = call.generate(body).await?;
        let raw = if doc.include_raw { Some(serde_json::to_value(&wire)?) } else { None };
        let mut resp = normalize_extract(&wire, model, &call.api_model, source.pdf_page_count())?;
        if request.citations {
            // Gemini returns no geometry, so per-field citations cannot be produced.
            resp.metadata.insert("gemini_citations_unsupported".into(), json!(true));
        }
        resp.raw = raw;
        Ok(resp)
    }
}

// ---- call plumbing ------------------------------------------------------------------------------

/// Everything resolved once per call: credentials, endpoints, deadline, retry policy.
struct Call {
    api_key: String,
    base: String,
    api_model: String,
    /// `generationConfig.thinkingConfig.thinkingLevel` a registry preset pins (e.g. `3.8-flash-low`).
    thinking_level: Option<&'static str>,
    deadline: Deadline,
    retry: Retry,
}

/// Never print the key, not even through `{:?}`.
impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("base", &self.base)
            .field("api_model", &self.api_model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Call {
    fn new(request: &DocumentRequest, model: &str) -> Result<Self> {
        let api_key = provider::resolve_api_key(request, ENV_KEY, NAME)?;
        let base = provider::resolve_base_url(request, ENV_BASE, DEFAULT_BASE);
        Ok(Self {
            api_key,
            base,
            api_model: api_model(request, model),
            thinking_level: thinking_level(model),
            deadline: Deadline::new(request.timeout_secs),
            retry: Retry::new(request.max_retries),
        })
    }

    /// Assemble the `generateContent` body: the document part, the prompt, and the output schema.
    /// `provider_options` (minus PuffinParse's own keys) is deep-merged in last, so callers can set
    /// `generationConfig.thinkingConfig`, `safetySettings`, `systemInstruction`, …
    fn body(&self, source: &Source, prompt: String, schema: Value, request: &DocumentRequest) -> Value {
        let mut body = json!({
            "contents": [{"role": "user", "parts": [source.part(), {"text": prompt}]}],
            "generationConfig": {
                "temperature": 0,
                "response_mime_type": "application/json",
                "response_schema": schema,
            },
        });
        if let Some(level) = self.thinking_level {
            body["generationConfig"]["thinkingConfig"] = json!({"thinkingLevel": level});
        }
        if let Some(Value::Object(opts)) = &request.provider_options {
            let mut opts = opts.clone();
            for own in ["model", "prompt", "prompt_suffix"] {
                opts.remove(own);
            }
            if !opts.is_empty() {
                deep_merge(&mut body, &Value::Object(opts));
            }
        }
        body
    }

    async fn generate(&self, body: Value) -> Result<GenerateContentResponse> {
        let client = http::client();
        let url = format!("{}/{API_VERSION}/models/{}:generateContent", self.base, self.api_model);
        let wire: GenerateContentResponse = http::with_retry(NAME, self.retry, &self.deadline, || {
            let rb = client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("content-type", "application/json")
                .timeout(self.deadline.request_timeout())
                .json(&body);
            async move { http::read_json(NAME, rb.send().await?).await }
        })
        .await?;
        Ok(wire)
    }

    /// Load the document and turn it into the content part Gemini wants.
    async fn prepare(&self, request: &DocumentRequest) -> Result<Source> {
        let (data, mime) = match provider::load_bytes(&request.input).await? {
            Some(data) => (data, request.input.mime_type()),
            None => {
                let DocumentInput::Url { url } = &request.input else {
                    return Err(Error::input("gemini: no document bytes"));
                };
                self.download(url, &request.input).await?
            }
        };
        if data.len() > MAX_INPUT_BYTES {
            return Err(Error::input(format!(
                "gemini: document is {} bytes, above the {MAX_INPUT_BYTES}-byte ceiling PuffinParse will buffer",
                data.len()
            )));
        }
        let mime = sniff_mime(&data, &mime);
        if data.len() > INLINE_MAX_BYTES {
            let uri = self.upload_file(&data, &mime, &request.input.filename()).await?;
            tracing::debug!(uri = %uri, bytes = data.len(), "gemini: uploaded via Files API");
            return Ok(Source::File { uri, mime });
        }
        Ok(Source::Inline { data, mime })
    }

    /// Gemini cannot fetch URLs itself, so PuffinParse downloads the bytes and inlines them.
    async fn download(&self, url: &str, input: &DocumentInput) -> Result<(bytes::Bytes, String)> {
        // Address-filtered, size-capped, and the response body never reaches an error message.
        let fetched = crate::fetch::fetch_document(NAME, url, &self.deadline, self.retry).await?;
        let mime = fetched.mime_or(input.mime_type());
        Ok((fetched.data, mime))
    }

    /// Resumable Files API upload: `start` → `upload, finalize` → wait for `ACTIVE`.
    async fn upload_file(&self, data: &bytes::Bytes, mime: &str, filename: &str) -> Result<String> {
        let client = http::client();
        let start_url = format!("{}/upload/{API_VERSION}/files", self.base);
        let len = data.len();
        let upload_url: String = http::with_retry(NAME, self.retry, &self.deadline, || {
            let rb = client
                .post(&start_url)
                .header("x-goog-api-key", &self.api_key)
                .header("X-Goog-Upload-Protocol", "resumable")
                .header("X-Goog-Upload-Command", "start")
                .header("X-Goog-Upload-Header-Content-Length", len.to_string())
                .header("X-Goog-Upload-Header-Content-Type", mime)
                .header("content-type", "application/json")
                .timeout(self.deadline.request_timeout())
                .json(&json!({"file": {"display_name": filename}}));
            async move {
                let resp = rb.send().await?;
                let url = resp
                    .headers()
                    .get("x-goog-upload-url")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                    .filter(|u| !u.is_empty());
                let status = resp.status();
                let body = resp.text().await?;
                if !status.is_success() {
                    return Err(Error::from_http(NAME, status.as_u16(), &body));
                }
                url.ok_or_else(|| {
                    Error::provider("files API start returned no x-goog-upload-url header").with_provider(NAME)
                })
            }
        })
        .await?;

        let file: FileEnvelope = http::with_retry(NAME, self.retry, &self.deadline, || {
            let rb = client
                .post(&upload_url)
                // reqwest sets Content-Length from the body; a hand-written one would be a duplicate.
                .header("X-Goog-Upload-Offset", "0")
                .header("X-Goog-Upload-Command", "upload, finalize")
                .timeout(self.deadline.request_timeout())
                .body(data.clone());
            async move { http::read_json(NAME, rb.send().await?).await }
        })
        .await?;
        let file = file.file;
        let uri =
            file.uri.clone().ok_or_else(|| Error::provider("files API returned no file uri").with_provider(NAME))?;

        // PDFs are usually ACTIVE immediately; wait when they are not.
        if file.state.as_deref().is_some_and(|s| s != "ACTIVE") {
            let name = file
                .name
                .clone()
                .ok_or_else(|| Error::provider("files API returned no file name").with_provider(NAME))?;
            let get_url = format!("{}/{API_VERSION}/{name}", self.base);
            http::poll_until(NAME, &self.deadline, Duration::from_millis(500), Duration::from_secs(5), || {
                let rb = client
                    .get(&get_url)
                    .header("x-goog-api-key", &self.api_key)
                    .timeout(self.deadline.request_timeout());
                async move {
                    let f: FileInfo = http::read_json(NAME, rb.send().await?).await?;
                    match f.state.as_deref() {
                        Some("ACTIVE") => Ok(Some(())),
                        Some("FAILED") => Err(Error::provider(format!(
                            "files API processing failed: {}",
                            f.error.map(|e| e.message).unwrap_or_default()
                        ))
                        .with_provider(NAME)),
                        _ => Ok(None),
                    }
                }
            })
            .await?;
        }
        Ok(uri)
    }
}

/// The document, ready to be attached to a request.
#[derive(Debug)]
enum Source {
    Inline { data: bytes::Bytes, mime: String },
    File { uri: String, mime: String },
}

impl Source {
    fn part(&self) -> Value {
        match self {
            Source::Inline { data, mime } => json!({"inline_data": {"mime_type": mime, "data": base64_encode(data)}}),
            Source::File { uri, mime } => json!({"file_data": {"file_uri": uri, "mime_type": mime}}),
        }
    }

    fn mime(&self) -> &str {
        match self {
            Source::Inline { mime, .. } | Source::File { mime, .. } => mime,
        }
    }

    fn is_pdf(&self) -> bool {
        self.mime() == "application/pdf"
    }

    /// Sanity check for `Usage.pages`: count `/Type /Page` objects in an inline PDF.
    fn pdf_page_count(&self) -> Option<u32> {
        match self {
            Source::Inline { data, .. } if self.is_pdf() => pdf_page_count(data),
            _ => None,
        }
    }
}

/// `gemini/2.5-flash` → API model `gemini-2.5-flash`; `provider_options.model` overrides it
/// verbatim so a model newer than the registry can still be reached. A thinking preset such as
/// `3.8-flash-low` calls its base model (`gemini-3.8-flash`).
fn api_model(request: &DocumentRequest, model: &str) -> String {
    match request.option("model").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        Some(m) => m.to_string(),
        None if thinking_level(model).is_some() => format!("gemini-{}", model.strip_suffix("-low").unwrap_or(model)),
        None => format!("gemini-{model}"),
    }
}

/// Registry presets that pin a Gemini 3 thinking level. `low` is the lowest level Gemini 3.8 Flash
/// accepts (`minimal` is 3.5/3.6 only); its default is `medium`.
/// Source: <https://ai.google.dev/gemini-api/docs/generate-content/thinking> (checked 2026-10-08).
fn thinking_level(model: &str) -> Option<&'static str> {
    match model {
        "3.8-flash-low" => Some("low"),
        _ => None,
    }
}

// ---- prompts and schemas -------------------------------------------------------------------------

const PARSE_RULES: &str = "\
Transcribe the attached document to GitHub-flavoured Markdown, page by page.

Rules:
- Return one entry in \"pages\" per page of the document, in reading order, with a 1-based \"page_number\".
- Transcribe every visible character. Do not summarise, translate, re-order, or add commentary.
- Headings become #/##/###; bullet and numbered lists keep their markers; tables become GFM pipe \
tables with a header separator row; formulas use $…$ (inline) or $$…$$ (display).
- Multi-column pages are read column by column, left to right.
- Describe images and charts inline as [Figure: short description].
- Keep headers, footers and page numbers where they appear on the page.
- Never wrap a page in ``` code fences unless the page really shows a code block.
- A blank page is an entry with an empty \"markdown\" string.";

fn parse_prompt(request: &DocumentRequest, source: &Source) -> String {
    if let Some(p) = request.option("prompt").and_then(Value::as_str) {
        return p.to_string();
    }
    let mut prompt = String::from(PARSE_RULES);
    // Whole-file processing: page selection is a prompt instruction, and only meaningful for PDFs.
    if let Some(pages) = &request.pages {
        if source.is_pdf() {
            prompt.push_str(&format!(
                "\n- Transcribe ONLY these pages of the PDF: {pages} (1-based, inclusive ranges). \
                 Skip every other page and keep the original page numbers in \"page_number\"."
            ));
        }
    }
    if let Some(lang) = &request.language {
        prompt.push_str(&format!("\n- The document is written in {lang}; transcribe it in that language."));
    }
    if request.output == OutputFormat::Text {
        prompt.push_str("\n- Plain-text output is wanted: keep the reading order but use no Markdown syntax.");
    }
    if let Some(extra) = request.option("prompt_suffix").and_then(Value::as_str) {
        prompt.push_str("\n\n");
        prompt.push_str(extra);
    }
    prompt
}

/// Structured-output schema for `parse`: an exact page split instead of one blob of markdown.
fn pages_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pages": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "page_number": {"type": "integer", "description": "1-based page number"},
                        "markdown": {"type": "string", "description": "the page transcribed to Markdown"},
                    },
                    "required": ["page_number", "markdown"],
                    "propertyOrdering": ["page_number", "markdown"],
                },
            },
        },
        "required": ["pages"],
    })
}

fn extract_prompt(request: &ExtractRequest, schema: &Value, source: &Source) -> String {
    if let Some(p) = request.document.option("prompt").and_then(Value::as_str) {
        return p.to_string();
    }
    let mut prompt = String::from(
        "Extract structured data from the attached document and return it as JSON matching this schema:\n",
    );
    prompt.push_str(&serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string()));
    prompt.push_str(
        "\n\nRules:\n\
         - Read values only from the document; never invent or infer a value that is not shown.\n\
         - Use null for anything the document does not state.\n\
         - Keep numbers as numbers (no currency symbols or thousands separators) and dates as they \
           appear unless the schema asks for a format.",
    );
    if let Some(pages) = &request.document.pages {
        if source.is_pdf() {
            prompt.push_str(&format!("\n- Use ONLY these pages of the PDF: {pages} (1-based)."));
        }
    }
    if let Some(instructions) = request.instructions.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nAdditional instructions:\n");
        prompt.push_str(instructions);
    }
    if let Some(extra) = request.document.option("prompt_suffix").and_then(Value::as_str) {
        prompt.push_str("\n\n");
        prompt.push_str(extra);
    }
    prompt
}

// ---- JSON Schema → Gemini schema subset -----------------------------------------------------------

/// Keywords Gemini's `response_schema` understands. Everything else is dropped.
const SCHEMA_KEEP: &[&str] = &[
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "enum",
    "items",
    "prefixItems",
    "properties",
    "required",
    "propertyOrdering",
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "anyOf",
];

/// Rewrite a JSON Schema (draft 2020-12) into the subset Gemini accepts.
///
/// * unsupported keywords (`$schema`, `additionalProperties`, `default`, `examples`, `allOf`, …) are dropped
/// * `type: ["string", "null"]` becomes `type: "string"` + `nullable: true`
/// * `oneOf` becomes `anyOf`; `const` becomes a one-value `enum`
/// * local `$ref`s into `$defs` / `definitions` are inlined (depth-limited, cycles collapse to a bare object)
/// * an object with `properties` but no `type` gets `type: "object"`
pub fn sanitize_schema(schema: &Value) -> Value {
    let defs = collect_defs(schema);
    sanitize_node(schema, &defs, 0)
}

fn collect_defs(schema: &Value) -> Map<String, Value> {
    let mut defs = Map::new();
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(o)) = schema.get(key) {
            for (k, v) in o {
                defs.insert(format!("{key}/{k}"), v.clone());
            }
        }
    }
    defs
}

fn sanitize_node(node: &Value, defs: &Map<String, Value>, depth: u32) -> Value {
    let Value::Object(obj) = node else {
        // A bare `true`/`false` schema (or anything odd) degrades to "any object".
        return json!({"type": "object"});
    };
    if depth > 12 {
        return json!({"type": "string", "description": "nesting depth exceeded"});
    }
    if let Some(Value::String(r)) = obj.get("$ref") {
        return match resolve_ref(r, defs) {
            Some(target) => sanitize_node(&target, defs, depth + 1),
            None => json!({"type": "object"}),
        };
    }
    let mut out = Map::new();
    let mut nullable = obj.get("nullable").and_then(Value::as_bool).unwrap_or(false);
    for (k, v) in obj {
        if !SCHEMA_KEEP.contains(&k.as_str()) {
            continue;
        }
        match k.as_str() {
            "type" => match v {
                // `type: ["string", "null"]` → nullable string.
                Value::Array(types) => {
                    let mut kept = None;
                    for t in types.iter().filter_map(Value::as_str) {
                        if t == "null" {
                            nullable = true;
                        } else if kept.is_none() {
                            kept = Some(t.to_string());
                        }
                    }
                    if let Some(t) = kept {
                        out.insert("type".into(), Value::String(t));
                    }
                }
                Value::String(t) if t == "null" => {
                    nullable = true;
                    out.insert("type".into(), Value::String("string".into()));
                }
                other => {
                    out.insert("type".into(), other.clone());
                }
            },
            "properties" => {
                let Value::Object(props) = v else { continue };
                let cleaned: Map<String, Value> =
                    props.iter().map(|(pk, pv)| (pk.clone(), sanitize_node(pv, defs, depth + 1))).collect();
                out.insert("properties".into(), Value::Object(cleaned));
            }
            "items" => {
                out.insert("items".into(), sanitize_node(v, defs, depth + 1));
            }
            "prefixItems" | "anyOf" => {
                let Value::Array(items) = v else { continue };
                let cleaned: Vec<Value> = items.iter().map(|i| sanitize_node(i, defs, depth + 1)).collect();
                out.insert(k.clone(), Value::Array(cleaned));
            }
            "required" | "propertyOrdering" => {
                let Value::Array(items) = v else { continue };
                let cleaned: Vec<Value> = items.iter().filter(|i| i.is_string()).cloned().collect();
                if !cleaned.is_empty() {
                    out.insert(k.clone(), Value::Array(cleaned));
                }
            }
            "nullable" => {}
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    // `oneOf` is not supported but means the same thing here; `const` is a single-value `enum`.
    if !out.contains_key("anyOf") {
        if let Some(Value::Array(items)) = obj.get("oneOf") {
            let cleaned: Vec<Value> = items.iter().map(|i| sanitize_node(i, defs, depth + 1)).collect();
            out.insert("anyOf".into(), Value::Array(cleaned));
        }
    }
    if !out.contains_key("enum") {
        if let Some(c) = obj.get("const") {
            out.insert("enum".into(), Value::Array(vec![c.clone()]));
            out.entry("type").or_insert_with(|| Value::String(json_type_of(c).into()));
        }
    }
    if !out.contains_key("type") {
        if out.contains_key("properties") {
            out.insert("type".into(), Value::String("object".into()));
        } else if out.contains_key("items") || out.contains_key("prefixItems") {
            out.insert("type".into(), Value::String("array".into()));
        } else if !out.contains_key("anyOf") {
            out.insert("type".into(), Value::String("string".into()));
        }
    }
    if nullable {
        out.insert("nullable".into(), Value::Bool(true));
    }
    Value::Object(out)
}

fn resolve_ref(reference: &str, defs: &Map<String, Value>) -> Option<Value> {
    let path = reference.strip_prefix("#/")?;
    defs.get(path).cloned()
}

fn json_type_of(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        _ => "string",
    }
}

// ---- wire types -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub prompt_feedback: Option<PromptFeedback>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Candidate {
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Content {
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Part {
    #[serde(default)]
    pub text: Option<String>,
    /// `true` on a thought-summary part (only present with `thinkingConfig.includeThoughts`).
    #[serde(default)]
    pub thought: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    /// Reasoning tokens — billed at the output rate but *not* included in `candidates_token_count`.
    #[serde(default)]
    pub thoughts_token_count: u32,
    #[serde(default)]
    pub cached_content_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptFeedback {
    #[serde(default)]
    pub block_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FileEnvelope {
    file: FileInfo,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FileInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<FileError>,
}

#[derive(Debug, Clone, Deserialize)]
struct FileError {
    #[serde(default)]
    message: String,
}

/// The JSON the model is asked to produce in `parse` mode.
#[derive(Debug, Clone, Deserialize)]
struct PagesPayload {
    #[serde(default)]
    pages: Vec<PagePayload>,
}

#[derive(Debug, Clone, Deserialize)]
struct PagePayload {
    #[serde(default)]
    page_number: Option<u32>,
    #[serde(default)]
    markdown: Option<String>,
    /// Accepted as an alias so a model that answers `{"text": …}` is not thrown away.
    #[serde(default)]
    text: Option<String>,
}

// ---- normalisation ---------------------------------------------------------------------------------

fn normalize_parse(
    wire: &GenerateContentResponse,
    fmt: OutputFormat,
    model: &str,
    api_model: &str,
    pdf_pages: Option<u32>,
) -> Result<ParseResponse> {
    let text = response_text(wire)?;
    let payload: PagesPayload = serde_json::from_str(strip_code_fence(&text)).map_err(|e| {
        Error::provider(format!("gemini returned non-schema JSON: {e}; body starts: {}", http::snippet(&text)))
            .with_provider(NAME)
    })?;
    let mut pages: Vec<Page> = Vec::with_capacity(payload.pages.len());
    for (i, p) in payload.pages.iter().enumerate() {
        let md = p.markdown.clone().or_else(|| p.text.clone()).unwrap_or_default().trim().to_string();
        let plain = crate::types::markdown_to_text(&md);
        let page_number = p.page_number.filter(|n| *n > 0).unwrap_or(i as u32 + 1);
        let content = match fmt {
            OutputFormat::Markdown => md.clone(),
            OutputFormat::Text => plain.clone(),
        };
        // Gemini reports no geometry: one text block per page, no bbox, no confidence — and no
        // block at all for a blank page.
        let mut page = super::vlm::text_page(page_number, content, plain);
        if page.markdown.is_empty() {
            page.blocks.clear();
        }
        pages.push(page);
    }
    if pages.is_empty() {
        return Err(Error::provider("gemini returned no pages").with_provider(NAME));
    }
    let counted = pages.len() as u32;
    let usage = usage_from(wire, counted, model);
    let mut resp = ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage);
    resp.provider_job_id = wire.response_id.clone();
    add_metadata(&mut resp.metadata, wire, api_model);
    if let Some(n) = pdf_pages {
        resp.metadata.insert("gemini_pdf_page_count".into(), json!(n));
        if n != counted {
            tracing::warn!(returned = counted, pdf_pages = n, "gemini: page count differs from the PDF's own count");
            resp.metadata.insert("gemini_page_count_mismatch".into(), json!(true));
        }
    }
    Ok(resp)
}

fn normalize_extract(
    wire: &GenerateContentResponse,
    model: &str,
    api_model: &str,
    pdf_pages: Option<u32>,
) -> Result<ExtractResponse> {
    let text = response_text(wire)?;
    let data: Value = serde_json::from_str(strip_code_fence(&text)).map_err(|e| {
        Error::provider(format!("gemini returned non-JSON data: {e}; body starts: {}", http::snippet(&text)))
            .with_provider(NAME)
    })?;
    // Whole-file processing: pages billed are the document's pages (1 for a single image).
    let usage = usage_from(wire, pdf_pages.unwrap_or(1).max(1), model);
    let mut resp = ExtractResponse::new(NAME, &format!("{NAME}/{model}"), data, usage);
    resp.provider_job_id = wire.response_id.clone();
    add_metadata(&mut resp.metadata, wire, api_model);
    Ok(resp)
}

fn add_metadata(
    metadata: &mut std::collections::BTreeMap<String, Value>,
    wire: &GenerateContentResponse,
    api_model: &str,
) {
    metadata.insert("gemini_api_model".into(), json!(api_model));
    if let Some(v) = &wire.model_version {
        metadata.insert("gemini_model_version".into(), json!(v));
    }
    if let Some(u) = &wire.usage_metadata {
        metadata.insert(
            "gemini_tokens".into(),
            json!({
                "prompt": u.prompt_token_count,
                "candidates": u.candidates_token_count,
                "thoughts": u.thoughts_token_count,
                "cached": u.cached_content_token_count,
                "total": u.total_token_count,
            }),
        );
    }
    if let Some(f) = wire.candidates.first().and_then(|c| c.finish_reason.as_deref()).filter(|f| *f != "STOP") {
        metadata.insert("gemini_finish_reason".into(), json!(f));
    }
}

fn usage_from(wire: &GenerateContentResponse, pages: u32, model: &str) -> Usage {
    Usage { pages, credits: None, provider_cost_usd: wire.usage_metadata.as_ref().and_then(|u| token_cost(model, u)) }
}

/// The text of the first candidate, with provider-side failures turned into errors.
fn response_text(wire: &GenerateContentResponse) -> Result<String> {
    if let Some(reason) = wire.prompt_feedback.as_ref().and_then(|f| f.block_reason.as_deref()) {
        return Err(
            Error::new(ErrorKind::BadRequest, format!("gemini blocked the prompt: {reason}")).with_provider(NAME)
        );
    }
    let Some(candidate) = wire.candidates.first() else {
        return Err(Error::provider("gemini returned no candidates").with_provider(NAME));
    };
    let text: String = candidate
        .content
        .as_ref()
        .map(|c| {
            c.parts
                .iter()
                .filter(|p| p.thought != Some(true))
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        let reason = candidate.finish_reason.as_deref().unwrap_or("unknown");
        let hint = match reason {
            "MAX_TOKENS" => {
                " — raise generationConfig.maxOutputTokens (or lower the thinking budget) \
                              via provider_options, or split the document"
            }
            "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" => " — adjust safetySettings via provider_options",
            _ => "",
        };
        return Err(
            Error::provider(format!("gemini returned no text (finishReason: {reason}){hint}")).with_provider(NAME)
        );
    }
    if candidate.finish_reason.as_deref() == Some("MAX_TOKENS") {
        return Err(Error::provider(
            "gemini hit the output token limit, so the JSON is truncated — raise \
             generationConfig.maxOutputTokens via provider_options or parse fewer pages per call",
        )
        .with_provider(NAME));
    }
    Ok(text)
}

// ---- token pricing ----------------------------------------------------------------------------------

/// Public list prices in USD per 1M tokens (paid tier), used to fill `usage.provider_cost_usd`
/// exactly from `usageMetadata` instead of the per-page estimate in `pricing.json`.
/// Source: <https://ai.google.dev/gemini-api/docs/pricing> (checked 2026-09-11; Gemini 3 Flash preview
/// added 2026-10-08).
struct TokenPrice {
    input: f64,
    output: f64,
    /// Long-prompt tier: `(prompt tokens above, input, output)`.
    long: Option<(u32, f64, f64)>,
}

fn token_price(model: &str) -> Option<TokenPrice> {
    Some(match model {
        "2.5-flash" => TokenPrice { input: 0.30, output: 2.50, long: None },
        "2.5-pro" => TokenPrice { input: 1.25, output: 10.00, long: Some((200_000, 2.50, 15.00)) },
        "2.5-flash-lite" => TokenPrice { input: 0.10, output: 0.40, long: None },
        "3.5-flash" => TokenPrice { input: 1.50, output: 9.00, long: None },
        "3.5-flash-lite" => TokenPrice { input: 0.30, output: 2.50, long: None },
        // Introductory pricing through 2026-12-31; the list rate doubles to 1.50 / 7.50 after that.
        "3.8-flash" | "3.8-flash-low" => TokenPrice { input: 0.75, output: 3.75, long: None },
        "3-flash-preview" => TokenPrice { input: 0.50, output: 3.00, long: None },
        _ => return None,
    })
}

/// Exact cost for one call: prompt tokens at the input rate, answer **and reasoning** tokens at the
/// output rate. Cached-context discounts are not modelled (PuffinParse never sends `cachedContent`).
fn token_cost(model: &str, usage: &UsageMetadata) -> Option<f64> {
    let p = token_price(model)?;
    let (input, output) = match p.long {
        Some((threshold, li, lo)) if usage.prompt_token_count > threshold => (li, lo),
        _ => (p.input, p.output),
    };
    let out_tokens = f64::from(usage.candidates_token_count) + f64::from(usage.thoughts_token_count);
    Some((f64::from(usage.prompt_token_count) * input + out_tokens * output) / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> GenerateContentResponse {
        let raw = std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture exists");
        serde_json::from_str(&raw).expect("fixture parses")
    }

    #[test]
    fn normalizes_parse_fixture() {
        let wire = fixture("gemini_parse_multipage.json");
        let resp = normalize_parse(&wire, OutputFormat::Markdown, "2.5-flash", "gemini-2.5-flash", Some(2)).unwrap();
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.pages[0].page_number, 1);
        assert!(resp.pages[0].markdown.starts_with("# A Short History of the Harbor"));
        assert_eq!(resp.pages[0].blocks.len(), 1);
        assert_eq!(resp.pages[0].blocks[0].block_type, BlockType::Text);
        assert!(resp.pages[0].blocks[0].bbox.is_none(), "gemini reports no geometry");
        assert!(resp.pages[1].markdown.contains("| Site | Samples |"));
        assert!(resp.text.contains("Hazel Bend"));
        assert_eq!(resp.pages[0].width, None);
        assert_eq!(resp.metadata["gemini_pdf_page_count"], json!(2));
        assert!(!resp.metadata.contains_key("gemini_page_count_mismatch"));
        // 1 809 prompt + (1 060 + 0) output tokens on 2.5-flash.
        let cost = resp.usage.provider_cost_usd.unwrap();
        assert!((cost - (1809.0 * 0.30 + 1060.0 * 2.50) / 1e6).abs() < 1e-12, "{cost}");
        assert_eq!(resp.provider_job_id.as_deref(), Some("HfPJaNXKO7OXm9IPk7P1kQc"));
    }

    #[test]
    fn parse_fixture_as_text_output() {
        let wire = fixture("gemini_parse_multipage.json");
        let resp = normalize_parse(&wire, OutputFormat::Text, "2.5-flash", "gemini-2.5-flash", None).unwrap();
        assert!(!resp.pages[0].markdown.contains('#'));
        assert_eq!(resp.pages[0].markdown, resp.pages[0].text);
    }

    #[test]
    fn page_count_mismatch_is_flagged() {
        let wire = fixture("gemini_parse_multipage.json");
        let resp = normalize_parse(&wire, OutputFormat::Markdown, "2.5-flash", "gemini-2.5-flash", Some(5)).unwrap();
        assert_eq!(resp.usage.pages, 2, "billed pages follow what the model returned");
        assert_eq!(resp.metadata["gemini_page_count_mismatch"], json!(true));
    }

    #[test]
    fn normalizes_extract_fixture() {
        let wire = fixture("gemini_extract_invoice.json");
        let resp = normalize_extract(&wire, "2.5-flash", "gemini-2.5-flash", None).unwrap();
        assert_eq!(resp.data["invoice_number"], "INV-9865");
        assert_eq!(resp.data["total_due"], 14667.43);
        assert_eq!(resp.data["line_items"].as_array().unwrap().len(), 6);
        assert_eq!(resp.usage.pages, 1);
        assert!(resp.usage.provider_cost_usd.unwrap() > 0.0);
        assert!(resp.fields.is_empty(), "gemini has no per-field citations");
        assert_eq!(resp.metadata["gemini_api_model"], json!("gemini-2.5-flash"));
    }

    #[test]
    fn maps_error_payload() {
        let body = r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let e = Error::from_http(NAME, 429, body);
        assert_eq!(e.kind, ErrorKind::RateLimit);
        assert_eq!(e.message, "Quota exceeded");
        assert!(e.retryable);
        let e = Error::from_http(
            NAME,
            400,
            r#"{"error":{"code":400,"message":"API key not valid","status":"INVALID_ARGUMENT"}}"#,
        );
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert_eq!(e.message, "API key not valid");
    }

    #[test]
    fn blocked_and_truncated_responses_error() {
        let blocked: GenerateContentResponse =
            serde_json::from_str(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#).unwrap();
        let e = response_text(&blocked).unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("SAFETY"));

        let empty: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates":[{"content":{"parts":[]},"finishReason":"MAX_TOKENS"}]}"#).unwrap();
        let e = response_text(&empty).unwrap_err();
        assert!(e.message.contains("maxOutputTokens"), "{e}");

        let truncated: GenerateContentResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"{\"pages\":[{"}]},"finishReason":"MAX_TOKENS"}]}"#,
        )
        .unwrap();
        assert!(response_text(&truncated).unwrap_err().message.contains("truncated"));

        let none: GenerateContentResponse = serde_json::from_str(r#"{"candidates":[]}"#).unwrap();
        assert!(response_text(&none).is_err());
    }

    #[test]
    fn skips_thought_summary_parts() {
        let wire: GenerateContentResponse = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[
                 {"text":"I should read the header first.","thought":true},
                 {"text":"{\"pages\":[{\"page_number\":1,\"markdown\":\"Hi\"}]}"}]},
                 "finishReason":"STOP"}]}"#,
        )
        .unwrap();
        let resp = normalize_parse(&wire, OutputFormat::Markdown, "2.5-flash", "gemini-2.5-flash", None).unwrap();
        assert_eq!(resp.markdown, "Hi");
    }

    #[test]
    fn concatenates_multiple_text_parts() {
        let wire: GenerateContentResponse = serde_json::from_str(
            r##"{"candidates":[{"content":{"parts":[{"text":"{\"pages\":[{\"page_number\":1,"},
                 {"text":"\"markdown\":\"# Hi\"}]}"}]},"finishReason":"STOP"}]}"##,
        )
        .unwrap();
        let resp = normalize_parse(&wire, OutputFormat::Markdown, "2.5-flash", "gemini-2.5-flash", None).unwrap();
        assert_eq!(resp.markdown, "# Hi");
        assert_eq!(resp.usage.provider_cost_usd, None, "no usageMetadata ⇒ fall back to the page price");
    }

    #[test]
    fn sanitize_strips_unsupported_keywords() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "urn:invoice",
            "type": "object",
            "additionalProperties": false,
            "title": "Invoice",
            "properties": {
                "number": {"type": "string", "default": "", "examples": ["INV-1"], "maxLength": 32},
                "total": {"type": ["number", "null"], "description": "grand total"},
                "paid": {"const": true},
                "currency": {"type": "string", "enum": ["USD", "EUR"]},
                "lines": {
                    "type": "array",
                    "items": {"type": "object", "additionalProperties": true,
                              "properties": {"qty": {"type": "integer", "minimum": 0}}},
                },
                "status": {"oneOf": [{"type": "string"}, {"type": "integer"}]}
            },
            "required": ["number"],
            "unevaluatedProperties": false
        });
        let s = sanitize_schema(&schema);
        assert_eq!(s["type"], "object");
        assert!(s.get("$schema").is_none() && s.get("$id").is_none());
        assert!(s.get("additionalProperties").is_none() && s.get("unevaluatedProperties").is_none());
        assert_eq!(s["title"], "Invoice");
        assert_eq!(s["required"], json!(["number"]));
        let p = &s["properties"];
        assert!(p["number"].get("default").is_none() && p["number"].get("examples").is_none());
        assert_eq!(p["number"]["maxLength"], 32);
        assert_eq!(p["total"]["type"], "number");
        assert_eq!(p["total"]["nullable"], true);
        assert_eq!(p["paid"]["enum"], json!([true]));
        assert_eq!(p["paid"]["type"], "boolean");
        assert_eq!(p["currency"]["enum"], json!(["USD", "EUR"]));
        assert!(p["lines"]["items"].get("additionalProperties").is_none());
        assert_eq!(p["lines"]["items"]["properties"]["qty"]["minimum"], 0);
        assert_eq!(p["status"]["anyOf"][1]["type"], "integer");
        assert!(p["status"].get("oneOf").is_none());
    }

    #[test]
    fn sanitize_inlines_refs_and_infers_types() {
        let schema = json!({
            "$defs": {"Line": {"type": "object", "properties": {"sku": {"type": "string"}}}},
            "properties": {
                "lines": {"type": "array", "items": {"$ref": "#/$defs/Line"}},
                "meta": {"properties": {"k": {"type": "string"}}},
                "missing": {"$ref": "#/$defs/Nope"}
            }
        });
        let s = sanitize_schema(&schema);
        assert_eq!(s["type"], "object", "type inferred from properties");
        assert_eq!(s["properties"]["lines"]["items"]["properties"]["sku"]["type"], "string");
        assert_eq!(s["properties"]["meta"]["type"], "object");
        assert_eq!(s["properties"]["missing"], json!({"type": "object"}));
        assert!(s.get("$defs").is_none());
    }

    #[test]
    fn sanitize_survives_cycles_and_scalars() {
        let schema = json!({"$defs": {"Node": {"type": "object",
            "properties": {"child": {"$ref": "#/$defs/Node"}}}}, "$ref": "#/$defs/Node"});
        let s = sanitize_schema(&schema);
        // Depth-limited: it terminates and still produces a valid schema.
        assert_eq!(s["type"], "object");
        assert!(s.to_string().len() < 4096);
        assert_eq!(sanitize_schema(&json!(true)), json!({"type": "object"}));
    }

    #[test]
    fn builds_body_with_prompt_and_options() {
        let req = DocumentRequest::from_path("a.pdf").provider_options(
            json!({"model": "gemini-9-ultra", "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}}),
        );
        let call = Call {
            api_key: "k".into(),
            base: DEFAULT_BASE.into(),
            api_model: api_model(&req, "2.5-flash"),
            thinking_level: None,
            deadline: Deadline::new(10.0),
            retry: Retry::new(0),
        };
        assert_eq!(call.api_model, "gemini-9-ultra", "provider_options.model wins");
        let source = Source::Inline { data: bytes::Bytes::from_static(b"%PDF-1.4"), mime: "application/pdf".into() };
        let body = call.body(&source, "prompt".into(), pages_schema(), &req);
        assert_eq!(body["contents"][0]["parts"][0]["inline_data"]["mime_type"], "application/pdf");
        assert_eq!(body["contents"][0]["parts"][0]["inline_data"]["data"], "JVBERi0xLjQ=");
        assert_eq!(body["contents"][0]["parts"][1]["text"], "prompt");
        assert_eq!(body["generationConfig"]["temperature"], 0);
        assert_eq!(body["generationConfig"]["response_mime_type"], "application/json");
        assert_eq!(body["generationConfig"]["response_schema"]["required"], json!(["pages"]));
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
        assert!(body["generationConfig"].get("model").is_none(), "puffinparse keys are not merged into the body");

        let file = Source::File { uri: "https://x/files/abc".into(), mime: "application/pdf".into() };
        let body = call.body(&file, "p".into(), json!({}), &DocumentRequest::from_path("a.pdf"));
        assert_eq!(body["contents"][0]["parts"][0]["file_data"]["file_uri"], "https://x/files/abc");
    }

    #[test]
    fn low_thinking_preset_calls_the_base_model_with_thinking_level_low() {
        let req = DocumentRequest::from_path("a.pdf");
        assert_eq!(api_model(&req, "3.8-flash-low"), "gemini-3.8-flash");
        assert_eq!(api_model(&req, "3-flash-preview"), "gemini-3-flash-preview");
        assert_eq!(thinking_level("3.8-flash"), None);
        let call = Call {
            api_key: "k".into(),
            base: DEFAULT_BASE.into(),
            api_model: api_model(&req, "3.8-flash-low"),
            thinking_level: thinking_level("3.8-flash-low"),
            deadline: Deadline::new(10.0),
            retry: Retry::new(0),
        };
        let source = Source::Inline { data: bytes::Bytes::from_static(b"%PDF-1.4"), mime: "application/pdf".into() };
        let body = call.body(&source, "p".into(), pages_schema(), &req);
        assert_eq!(body["generationConfig"]["thinkingConfig"], json!({"thinkingLevel": "low"}));
        // A caller can still raise it.
        let req = req.provider_options(json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}}}));
        let body = call.body(&source, "p".into(), pages_schema(), &req);
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
        // Same token prices as the base model.
        let u = UsageMetadata { prompt_token_count: 1000, candidates_token_count: 100, ..Default::default() };
        assert_eq!(token_cost("3.8-flash-low", &u), token_cost("3.8-flash", &u));
        let preview = token_cost("3-flash-preview", &u).unwrap();
        assert!((preview - (1000.0 * 0.50 + 100.0 * 3.00) / 1e6).abs() < 1e-12);
    }

    #[test]
    fn prompt_mentions_pages_only_for_pdfs() {
        let pdf = Source::Inline { data: bytes::Bytes::new(), mime: "application/pdf".into() };
        let png = Source::Inline { data: bytes::Bytes::new(), mime: "image/png".into() };
        let req = DocumentRequest::from_path("a.pdf").pages("2-3").language("de");
        let p = parse_prompt(&req, &pdf);
        assert!(p.contains("ONLY these pages of the PDF: 2-3"));
        assert!(p.contains("written in de"));
        assert!(!parse_prompt(&req, &png).contains("ONLY these pages"));
        let custom = DocumentRequest::from_path("a.pdf").provider_options(json!({"prompt": "just do it"}));
        assert_eq!(parse_prompt(&custom, &pdf), "just do it");
        let suffixed = DocumentRequest::from_path("a.pdf").provider_options(json!({"prompt_suffix": "Keep stamps."}));
        assert!(parse_prompt(&suffixed, &pdf).ends_with("Keep stamps."));
    }

    #[test]
    fn extract_prompt_carries_schema_and_instructions() {
        let doc = DocumentRequest::from_path("a.pdf");
        let req = ExtractRequest::new(doc, json!({"type": "object"})).instructions("Totals include tax.");
        let schema = sanitize_schema(&req.schema);
        let p = extract_prompt(&req, &schema, &Source::Inline { data: bytes::Bytes::new(), mime: "image/png".into() });
        assert!(p.contains("Totals include tax."));
        assert!(p.contains("\"type\": \"object\""));
        assert!(p.contains("never invent"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xFF, 0xD8, 0xFF]), "/9j/");
    }

    #[test]
    fn sniffs_mime_from_magic_bytes() {
        assert_eq!(sniff_mime(b"%PDF-1.7\n", "application/octet-stream"), "application/pdf");
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 13, 10], "text/plain"), "image/png");
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0], ""), "image/jpeg");
        assert_eq!(sniff_mime(b"RIFF____WEBPVP8 ", ""), "image/webp");
        assert_eq!(sniff_mime(b"hello", "text/markdown"), "text/markdown");
        assert_eq!(sniff_mime(b"hello", "application/octet-stream"), "text/plain");
    }

    #[test]
    fn counts_pdf_pages() {
        let pdf = b"%PDF-1.4\n1 0 obj<</Type /Pages /Kids[2 0 R 3 0 R]/Count 2>>endobj\n\
                    2 0 obj<</Type/Page /Parent 1 0 R>>endobj\n3 0 obj<</Type /Page>>endobj";
        assert_eq!(pdf_page_count(pdf), Some(2));
        assert_eq!(pdf_page_count(b"%PDF-1.4 no pages here"), None);
        assert_eq!(pdf_page_count(b"<</Type /Pages>>"), None, "/Pages is not a page");
    }

    #[test]
    fn token_cost_uses_thoughts_and_long_context_tier() {
        let u = UsageMetadata { prompt_token_count: 1000, candidates_token_count: 500, ..Default::default() };
        assert!((token_cost("2.5-flash", &u).unwrap() - (1000.0 * 0.30 + 500.0 * 2.50) / 1e6).abs() < 1e-12);
        let thinking = UsageMetadata { thoughts_token_count: 400, ..u.clone() };
        assert!(token_cost("2.5-flash", &thinking).unwrap() > token_cost("2.5-flash", &u).unwrap());
        let long = UsageMetadata { prompt_token_count: 250_000, candidates_token_count: 100, ..Default::default() };
        assert!((token_cost("2.5-pro", &long).unwrap() - (250_000.0 * 2.50 + 100.0 * 15.00) / 1e6).abs() < 1e-9);
        assert_eq!(token_cost("4.0-nope", &u), None);
    }

    #[test]
    fn strips_stray_code_fences() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n{\"a\":1}```"), "{\"a\":1}");
        assert_eq!(strip_code_fence(" {\"a\":1} "), "{\"a\":1}");
    }

    #[test]
    fn missing_key_is_an_authentication_error() {
        let req = DocumentRequest::from_path("a.pdf").api_key(" ");
        // With the env var set this resolves; only the empty-and-unset case can be asserted portably.
        if std::env::var(ENV_KEY).is_err() {
            let e = Call::new(&req, "2.5-flash").unwrap_err();
            assert_eq!(e.kind, ErrorKind::Authentication);
            assert!(e.message.contains(ENV_KEY));
        }
    }

    #[test]
    fn base_url_is_overridable() {
        let req = DocumentRequest::from_path("a.pdf").api_key("k").base_url("https://proxy.local/gemini/");
        let call = Call::new(&req, "2.5-flash").unwrap();
        assert_eq!(call.base, "https://proxy.local/gemini");
        assert_eq!(call.api_model, "gemini-2.5-flash");
    }
}

// ---- loopback transport tests --------------------------------------------------------------------

/// End-to-end tests against a hand-rolled HTTP/1.1 server on 127.0.0.1: they exercise the real
/// request the provider builds (URL, headers, body) and the real response path, without network.
#[cfg(test)]
mod transport {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One captured request: the request line plus headers, and the body.
    type Captured = Arc<Mutex<Vec<(String, String)>>>;

    /// Serve `responses` (status, body) in order, recording what was asked for.
    async fn serve(responses: Vec<(u16, String)>) -> (String, Captured) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut head_end = None;
                let mut content_length = 0usize;
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if head_end.is_none() {
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_end = Some(pos + 4);
                            let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            content_length = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }
                    if let Some(end) = head_end {
                        if buf.len() >= end + content_length {
                            break;
                        }
                    }
                }
                let end = head_end.unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..end.saturating_sub(4)]).to_string();
                let body_in = String::from_utf8_lossy(&buf[end.min(buf.len())..]).to_string();
                sink.lock().expect("lock").push((head, body_in));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), captured)
    }

    fn fixture_body() -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/gemini_parse_multipage.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture")
    }

    #[tokio::test]
    async fn sends_the_documented_request_and_parses_the_answer() {
        let (base, captured) = serve(vec![(200, fixture_body())]).await;
        let pdf = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let req = DocumentRequest::from_path(pdf).api_key("test-key").base_url(&base).timeout_secs(20.0);
        let resp = Gemini.parse(&req, "2.5-flash").await.expect("parse");

        let calls = captured.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        let (head, body) = &calls[0];
        assert!(head.starts_with("POST /v1beta/models/gemini-2.5-flash:generateContent HTTP/1.1"), "{head}");
        assert!(head.to_lowercase().contains("x-goog-api-key: test-key"), "{head}");
        let sent: Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(sent["contents"][0]["parts"][0]["inline_data"]["mime_type"], "application/pdf");
        let data = sent["contents"][0]["parts"][0]["inline_data"]["data"].as_str().expect("base64");
        assert!(data.starts_with("JVBERi0xLjQ"), "base64 of %PDF-1.4: {}", &data[..12]);
        assert!(sent["contents"][0]["parts"][1]["text"].as_str().expect("prompt").contains("page by page"));
        assert_eq!(sent["generationConfig"]["response_mime_type"], "application/json");

        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.metadata["gemini_pdf_page_count"], json!(2));
        assert!(resp.usage.provider_cost_usd.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let err = r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let (base, captured) = serve(vec![(429, err.into()), (200, fixture_body())]).await;
        let req = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"), "p.png")
            .api_key("k")
            .base_url(&base)
            .timeout_secs(20.0)
            .max_retries(2);
        let resp = Gemini.parse(&req, "2.5-flash").await.expect("parse after retry");
        assert_eq!(captured.lock().expect("lock").len(), 2, "the 429 was retried");
        assert_eq!(resp.pages.len(), 2);
    }

    #[tokio::test]
    async fn maps_a_400_to_bad_request() {
        let err = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}"#;
        let (base, _) = serve(vec![(400, err.into())]).await;
        let req = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"%PDF-1.4"), "a.pdf")
            .api_key("bad")
            .base_url(&base)
            .timeout_secs(20.0)
            .max_retries(0);
        let e = Gemini.parse(&req, "2.5-flash").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert_eq!(e.status_code, Some(400));
        assert!(e.message.contains("API key not valid"));
    }

    #[tokio::test]
    async fn extract_sends_the_sanitized_schema() {
        let data = r#"{"invoice_number":"INV-9865","total_due":14667.43}"#;
        let body = json!({
            "candidates": [{"content": {"parts": [{"text": data}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20, "totalTokenCount": 120},
            "modelVersion": "gemini-2.5-flash"
        });
        let (base, captured) = serve(vec![(200, body.to_string())]).await;
        let doc = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"%PDF-1.4"), "a.pdf")
            .api_key("k")
            .base_url(&base)
            .timeout_secs(20.0);
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {"invoice_number": {"type": "string"}, "total_due": {"type": ["number", "null"]}},
            "required": ["invoice_number"]
        });
        let resp = Gemini
            .extract(&ExtractRequest::new(doc, schema).instructions("Amounts are in USD."), "2.5-flash")
            .await
            .expect("extract");
        let calls = captured.lock().expect("lock");
        let sent: Value = serde_json::from_str(&calls[0].1).expect("json");
        let schema_sent = &sent["generationConfig"]["response_schema"];
        assert!(schema_sent.get("$schema").is_none() && schema_sent.get("additionalProperties").is_none());
        assert_eq!(schema_sent["properties"]["total_due"]["nullable"], true);
        assert!(sent["contents"][0]["parts"][1]["text"].as_str().expect("prompt").contains("Amounts are in USD."));
        assert_eq!(resp.data["invoice_number"], "INV-9865");
        assert_eq!(resp.usage.pages, 1);
    }
}

// ---- live tests ---------------------------------------------------------------------------------------

/// Live calls against the real API. Ignored by default:
/// `GEMINI_API_KEY=… cargo test -p puffinparse-core gemini_live -- --ignored --nocapture`
#[cfg(test)]
mod live {
    use super::*;

    const DOCS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs");

    fn key_set() -> bool {
        let ok = std::env::var(ENV_KEY).map(|v| !v.trim().is_empty()).unwrap_or(false);
        if !ok {
            eprintln!("skipping: {ENV_KEY} not set");
        }
        ok
    }

    #[tokio::test]
    #[ignore = "needs GEMINI_API_KEY and network"]
    async fn gemini_live_parse_image() {
        if !key_set() {
            return;
        }
        let req = DocumentRequest::from_path(format!("{DOCS}/invoice_001.png")).timeout_secs(180.0);
        let resp = Gemini.parse(&req, "2.5-flash").await.expect("parse succeeds");
        eprintln!("pages={} cost={:?} md:\n{}", resp.pages.len(), resp.usage.provider_cost_usd, resp.markdown);
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.usage.pages, 1);
        assert!(resp.markdown.contains("Cedar Ridge Supply"));
        assert!(resp.markdown.contains("14,667.43") || resp.markdown.contains("14667.43"));
        assert!(resp.usage.provider_cost_usd.unwrap() > 0.0);
    }

    #[tokio::test]
    #[ignore = "needs GEMINI_API_KEY and network"]
    async fn gemini_live_parse_multipage_pdf() {
        if !key_set() {
            return;
        }
        let req = DocumentRequest::from_path(format!("{DOCS}/multipage_001.pdf")).timeout_secs(180.0);
        let resp = Gemini.parse(&req, "2.5-flash").await.expect("parse succeeds");
        eprintln!("pages={} cost={:?}", resp.pages.len(), resp.usage.provider_cost_usd);
        assert_eq!(resp.pages.len(), 2, "the PDF has two pages and the schema keeps them apart");
        assert_eq!(resp.pages[1].page_number, 2);
        assert_eq!(resp.metadata["gemini_pdf_page_count"], json!(2));
        assert!(resp.markdown.contains("A Short History of the Harbor"));
        assert!(resp.pages[1].markdown.contains("Hazel Bend"));
    }

    #[tokio::test]
    #[ignore = "needs GEMINI_API_KEY and network"]
    async fn gemini_live_parse_pdf_page_selection() {
        if !key_set() {
            return;
        }
        let req = DocumentRequest::from_path(format!("{DOCS}/multipage_001.pdf")).pages("2").timeout_secs(180.0);
        let resp = Gemini.parse(&req, "2.5-flash").await.expect("parse succeeds");
        assert_eq!(resp.pages.len(), 1, "best-effort page selection via the prompt");
        assert_eq!(resp.pages[0].page_number, 2);
    }

    #[tokio::test]
    #[ignore = "needs GEMINI_API_KEY and network"]
    async fn gemini_live_ocr_derives_text() {
        if !key_set() {
            return;
        }
        let req = DocumentRequest::from_path(format!("{DOCS}/invoice_001.png")).timeout_secs(180.0);
        let resp = Gemini.ocr(&req, "2.5-flash-lite").await.expect("ocr succeeds");
        assert_eq!(resp.metadata["puffinparse_derived_from"], json!("parse"));
        assert!(!resp.pages[0].lines.is_empty());
        assert!(resp.text.contains("Cedar Ridge Supply"));
    }

    #[tokio::test]
    #[ignore = "needs GEMINI_API_KEY and network"]
    async fn gemini_live_extract_invoice() {
        if !key_set() {
            return;
        }
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "vendor": {"type": "string"},
                "invoice_number": {"type": "string"},
                "invoice_date": {"type": "string", "format": "date"},
                "total_due": {"type": "number"},
                "line_items": {
                    "type": "array",
                    "items": {"type": "object", "properties": {
                        "description": {"type": "string"},
                        "quantity": {"type": "integer"},
                        "amount": {"type": "number"}}}
                }
            },
            "required": ["vendor", "invoice_number", "total_due"]
        });
        let doc = DocumentRequest::from_path(format!("{DOCS}/invoice_001.png")).timeout_secs(180.0);
        let req = ExtractRequest::new(doc, schema).instructions("Amounts are in USD.");
        let resp = Gemini.extract(&req, "2.5-flash").await.expect("extract succeeds");
        eprintln!("data={}", serde_json::to_string_pretty(&resp.data).unwrap());
        assert_eq!(resp.data["invoice_number"], "INV-9865");
        assert!((resp.data["total_due"].as_f64().unwrap() - 14667.43).abs() < 0.01);
        assert_eq!(resp.data["line_items"].as_array().unwrap().len(), 6);
    }
}
