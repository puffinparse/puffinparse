//! Upstage Document Parse (document digitization).
//!
//! Flow (sync, ≤ 100 pages): `POST /v1/document-digitization` as `multipart/form-data` with the
//! `document` file part and `model=document-parse`. With `provider_options.async = true` (up to
//! 1 000 pages): `POST /v1/document-digitization/async` → `{request_id}`, poll
//! `GET /v1/document-digitization/requests/{id}` until `completed`, then download every batch's
//! `download_url` (each holds the same JSON shape as the sync response) and concatenate them.
//!
//! Docs: <https://console.upstage.ai/docs/capabilities/document-digitization/document-parsing>

use crate::error::{Error, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
use crate::types::{
    pages_from_blocks, BBox, Block, BlockType, DocumentInput, DocumentRequest, OutputFormat, Page, ParseResponse, Usage,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

pub const NAME: &str = "upstage";
const ENV_KEY: &str = "UPSTAGE_API_KEY";
const ENV_BASE: &str = "UPSTAGE_BASE_URL";
const DEFAULT_BASE: &str = "https://api.upstage.ai";

/// Upstage Document Parse.
#[derive(Debug, Default, Clone, Copy)]
pub struct Upstage;

#[async_trait]
impl Provider for Upstage {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn parse(&self, request: &DocumentRequest, model: &str) -> Result<ParseResponse> {
        let api_key = provider::resolve_api_key(request, ENV_KEY, NAME)?;
        let base = provider::resolve_base_url(request, ENV_BASE, DEFAULT_BASE);
        let deadline = Deadline::new(request.timeout_secs);
        let retry = Retry::new(request.max_retries);
        let client = http::client();
        let fields = form_fields(request, model)?;

        // Upstage has no URL input: fetch the document ourselves and upload the bytes.
        let data = match provider::load_bytes(&request.input).await? {
            Some(bytes) => bytes,
            None => {
                let DocumentInput::Url { url } = &request.input else { unreachable!() };
                download(url, &deadline, retry).await?
            }
        };

        let use_async = request.option("async").and_then(Value::as_bool).unwrap_or(false);
        let submit_url = if use_async {
            format!("{base}/v1/document-digitization/async")
        } else {
            format!("{base}/v1/document-digitization")
        };

        let send = || {
            let mut form = reqwest::multipart::Form::new();
            for (k, v) in &fields {
                form = form.text(k.clone(), v.clone());
            }
            form = form.part("document", provider::file_part(data.clone(), &request.input));
            let rb = client
                .post(&submit_url)
                .bearer_auth(&api_key)
                .header("accept", "application/json")
                .timeout(deadline.request_timeout())
                .multipart(form);
            async move { rb.send().await }
        };

        let (result, request_id) = if use_async {
            let submit: AsyncSubmit = http::with_retry(NAME, retry, &deadline, || {
                let fut = send();
                async move { http::read_json(NAME, fut.await?).await }
            })
            .await?;
            let id = submit
                .id()
                .ok_or_else(|| Error::provider("async submit returned no request_id").with_provider(NAME))?;
            tracing::debug!(request_id = %id, "upstage: async request submitted");
            let status = poll_async(client, &base, &api_key, &id, &deadline).await?;
            let merged =
                download_batches(client, &status, &deadline, retry).await.map_err(|e| e.with_job_id(id.clone()))?;
            (merged, Some(id))
        } else {
            let result: DigitizationResult = http::with_retry(NAME, retry, &deadline, || {
                let fut = send();
                async move { http::read_json(NAME, fut.await?).await }
            })
            .await?;
            (result, None)
        };

        let raw = if request.include_raw { Some(serde_json::to_value(&result)?) } else { None };
        let mut resp = normalize(&result, request.output, model);
        // Document Parse has no page-range parameter, so a `pages` selection is applied here.
        // `usage.pages` stays at the billed count for the whole document.
        if let Some(spec) = &request.pages {
            let ranges = crate::util::parse_page_ranges(spec)?;
            resp = filter_pages(resp, &ranges);
            resp.metadata.insert("upstage_pages_filtered_client_side".into(), json!(spec));
        }
        resp.provider_job_id = request_id;
        resp.raw = raw;
        Ok(resp)
    }
}

/// Multipart text fields for the digitization call.
fn form_fields(request: &DocumentRequest, model: &str) -> Result<Vec<(String, String)>> {
    match model {
        "document-parse" | "document-parse-nightly" => {}
        other => return Err(Error::unsupported_model(format!("upstage: unknown model '{other}'"))),
    }
    let mut fields: Vec<(String, String)> = vec![
        ("model".into(), model.into()),
        // All three so the unified response has both markdown and a provider-supplied plain text.
        ("output_formats".into(), r#"["html","markdown","text"]"#.into()),
        ("ocr".into(), "auto".into()),
        ("coordinates".into(), "true".into()),
    ];
    if let Some(Value::Object(opts)) = &request.provider_options {
        for (k, v) in opts {
            if k == "async" {
                continue; // selects the endpoint, not a form field
            }
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Null => continue,
                // Arrays/objects go over the wire as JSON, e.g. base64_encoding=["table"].
                other => other.to_string(),
            };
            fields.retain(|(key, _)| key != k);
            fields.push((k.clone(), s));
        }
    }
    Ok(fields)
}

/// Fetch a URL input so it can be uploaded (Upstage accepts uploads only).
async fn download(url: &str, deadline: &Deadline, retry: Retry) -> Result<bytes::Bytes> {
    Ok(crate::fetch::fetch_document(NAME, url, deadline, retry).await?.data)
}

async fn poll_async(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    request_id: &str,
    deadline: &Deadline,
) -> Result<AsyncStatus> {
    let status = http::poll_until(NAME, deadline, Duration::from_secs(2), Duration::from_secs(15), || {
        let rb = client
            .get(format!("{base}/v1/document-digitization/requests/{request_id}"))
            .bearer_auth(api_key)
            .timeout(deadline.request_timeout());
        async move {
            let s: AsyncStatus = http::read_json(NAME, rb.send().await?).await?;
            Ok(if is_terminal(&s.status) { Some(s) } else { None })
        }
    })
    .await
    .map_err(|e| e.with_job_id(request_id.to_string()))?;
    if status.status != "completed" {
        let msg = status.failure_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into());
        return Err(Error::provider(format!("request {}: {msg}", status.status))
            .with_provider(NAME)
            .with_job_id(request_id.to_string()));
    }
    Ok(status)
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

/// Download every completed batch and concatenate them into one sync-shaped result.
async fn download_batches(
    client: &reqwest::Client,
    status: &AsyncStatus,
    deadline: &Deadline,
    retry: Retry,
) -> Result<DigitizationResult> {
    let mut batches: Vec<&Batch> = status.batches.iter().collect();
    batches.sort_by_key(|b| b.id);
    let mut merged = DigitizationResult { model: status.model.clone(), ..Default::default() };
    let mut markdown = Vec::new();
    let mut text = Vec::new();
    let mut html = Vec::new();
    for batch in batches {
        if batch.status != "completed" {
            let msg = batch.failure_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into());
            return Err(Error::provider(format!("batch {} {}: {msg}", batch.id, batch.status)).with_provider(NAME));
        }
        let Some(url) = batch.download_url.clone().filter(|u| !u.is_empty()) else {
            return Err(Error::provider(format!("batch {} has no download_url", batch.id)).with_provider(NAME));
        };
        // Presigned URL: no auth header, valid for ~15 minutes.
        let part: DigitizationResult = http::with_retry(NAME, retry, deadline, || {
            let rb = client.get(&url).timeout(deadline.request_timeout());
            async move { http::read_json(NAME, rb.send().await?).await }
        })
        .await?;
        // Batches are 10-page chunks. Page numbers are global, but if a batch numbers its own
        // pages from 1 while starting later in the document, shift it into document space.
        let min_page = part.elements.iter().filter_map(|e| e.page).min().unwrap_or(1);
        let offset = if batch.start_page > 1 && min_page <= 1 { batch.start_page - 1 } else { 0 };
        for mut el in part.elements {
            el.page = Some(el.page.unwrap_or(1) + offset);
            merged.elements.push(el);
        }
        if let Some(c) = &part.content {
            push_if_set(&mut markdown, c.markdown.as_deref());
            push_if_set(&mut text, c.text.as_deref());
            push_if_set(&mut html, c.html.as_deref());
        }
        if merged.api.is_none() {
            merged.api = part.api.clone();
        }
    }
    merged.content = Some(Content { html: join_opt(html), markdown: join_opt(markdown), text: join_opt(text) });
    merged.usage = Some(UsageWire { pages: status.total_pages.unwrap_or(0), ..Default::default() });
    Ok(merged)
}

fn push_if_set(out: &mut Vec<String>, s: Option<&str>) {
    if let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) {
        out.push(s.to_string());
    }
}

fn join_opt(parts: Vec<String>) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Keep only the pages inside `ranges` (1-based, `None` end = open-ended).
fn filter_pages(resp: ParseResponse, ranges: &[(u32, Option<u32>)]) -> ParseResponse {
    let keep = |p: u32| ranges.iter().any(|&(s, e)| p >= s && e.map(|e| p <= e).unwrap_or(true));
    let pages: Vec<Page> = resp.pages.into_iter().filter(|p| keep(p.page_number)).collect();
    let mut out = ParseResponse::from_pages(&resp.provider, &resp.model, pages, resp.usage);
    out.metadata = resp.metadata;
    out
}

// ---- wire types -------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DigitizationResult {
    /// API version; the sync response uses `api`, some payloads `apiVersion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, rename = "apiVersion", skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// Resolved model snapshot, e.g. `document-parse-260630`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageWire>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Content {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Element {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(default)]
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default)]
    pub content: Content,
    #[serde(default)]
    pub coordinates: Vec<Point>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct UsageWire {
    #[serde(default)]
    pub pages: u32,
    /// 1-based page numbers processed in `standard` mode (omitted when empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enhanced: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Deserialize)]
struct AsyncSubmit {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

impl AsyncSubmit {
    fn id(&self) -> Option<String> {
        self.request_id.clone().or_else(|| self.id.clone()).filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct AsyncStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    failure_message: Option<String>,
    #[serde(default)]
    total_pages: Option<u32>,
    #[serde(default)]
    batches: Vec<Batch>,
}

#[derive(Debug, Clone, Deserialize)]
struct Batch {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    status: String,
    #[serde(default)]
    failure_message: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default = "one")]
    start_page: u32,
}

fn one() -> u32 {
    1
}

// ---- normalisation -----------------------------------------------------------------------------

/// Upstage layout categories → unified block types.
fn map_category(category: &str) -> BlockType {
    match category {
        "paragraph" => BlockType::Text,
        "heading1" => BlockType::Title,
        "table" => BlockType::Table,
        "figure" | "chart" => BlockType::Figure,
        "caption" => BlockType::Caption,
        "list" => BlockType::List,
        "header" => BlockType::Header,
        "footer" => BlockType::Footer,
        "footnote" => BlockType::Footnote,
        "equation" => BlockType::Formula,
        // A code block is still text; `index` (table of contents) has no unified equivalent.
        "code" => BlockType::Text,
        _ => BlockType::Other,
    }
}

/// Four relative corner points → a normalised box.
fn bbox_from_coordinates(points: &[Point]) -> Option<BBox> {
    if points.is_empty() {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(BBox { x0: x0.clamp(0.0, 1.0), y0: y0.clamp(0.0, 1.0), x1: x1.clamp(0.0, 1.0), y1: y1.clamp(0.0, 1.0) })
}

pub(crate) fn normalize(result: &DigitizationResult, fmt: OutputFormat, model: &str) -> ParseResponse {
    let blocks: Vec<Block> = result
        .elements
        .iter()
        .map(|el| {
            let md = el
                .content
                .markdown
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| el.content.text.clone())
                .or_else(|| el.content.html.clone())
                .unwrap_or_default();
            let text = el.content.text.clone().filter(|s| !s.trim().is_empty());
            let content = match fmt {
                OutputFormat::Markdown => md.clone(),
                OutputFormat::Text => text.clone().unwrap_or_else(|| crate::types::markdown_to_text(&md)),
            };
            Block {
                block_type: map_category(&el.category),
                content: content.trim().to_string(),
                text: text.map(|t| t.trim().to_string()),
                bbox: bbox_from_coordinates(&el.coordinates),
                confidence: None,
                page_number: el.page.unwrap_or(1).max(1),
            }
        })
        .collect();

    let mut pages = pages_from_blocks(blocks, &BTreeMap::new());
    if pages.is_empty() {
        // No elements (e.g. `coordinates=false` plus an empty layout): fall back to whole-document
        // content so a caller still gets the text.
        if let Some(c) = &result.content {
            let markdown = c.markdown.clone().unwrap_or_default();
            let text = c.text.clone().unwrap_or_else(|| crate::types::markdown_to_text(&markdown));
            if !markdown.trim().is_empty() || !text.trim().is_empty() {
                let markdown = match fmt {
                    OutputFormat::Markdown => markdown,
                    OutputFormat::Text => text.clone(),
                };
                pages.push(Page {
                    page_number: 1,
                    width: None,
                    height: None,
                    markdown: markdown.trim().to_string(),
                    text: text.trim().to_string(),
                    blocks: Vec::new(),
                });
            }
        }
    }

    let billed = result.usage.as_ref().map(|u| u.pages).filter(|&p| p > 0).unwrap_or(pages.len() as u32);
    let usage = Usage { pages: billed, credits: None, provider_cost_usd: None };
    let mut resp = ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage);
    if let Some(m) = &result.model {
        resp.metadata.insert("upstage_model_version".into(), json!(m));
    }
    if let Some(enhanced) = result.usage.as_ref().and_then(|u| u.enhanced.as_ref()).filter(|e| !e.is_empty()) {
        resp.metadata.insert("upstage_enhanced_pages".into(), json!(enhanced));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> DigitizationResult {
        let raw = include_str!("../../tests/fixtures/upstage_document_parse.json");
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn normalizes_fixture() {
        let resp = normalize(&fixture(), OutputFormat::Markdown, "document-parse");
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.provider, NAME);
        assert_eq!(resp.model, "upstage/document-parse");
        assert_eq!(resp.metadata["upstage_model_version"], "document-parse-260630");
        assert_eq!(resp.pages[0].blocks[0].block_type, BlockType::Title);
        assert_eq!(resp.pages[0].blocks[0].content, "# Hello LiteOCR");
        assert_eq!(resp.pages[0].blocks[1].block_type, BlockType::Text);
        assert!(resp.pages[1].blocks.iter().any(|b| b.block_type == BlockType::Table && b.content.contains("| Item")));
        assert!(resp.pages[1].blocks.iter().any(|b| b.block_type == BlockType::Figure));
        // Document markdown is the pages joined in order.
        assert!(resp.markdown.starts_with("# Hello LiteOCR"));
        assert!(resp.markdown.contains("| Widget |"));
        assert!(resp.text.contains("Invoice #1234"));
    }

    #[test]
    fn bboxes_are_normalised() {
        let resp = normalize(&fixture(), OutputFormat::Markdown, "document-parse");
        let bb = resp.pages[0].blocks[0].bbox.unwrap();
        assert!((bb.x0 - 0.125).abs() < 1e-9, "{bb:?}");
        assert!((bb.y1 - 0.1052).abs() < 1e-9, "{bb:?}");
        for p in &resp.pages {
            for b in &p.blocks {
                let bb = b.bbox.expect("every fixture element has coordinates");
                assert!((0.0..=1.0).contains(&bb.x0) && (0.0..=1.0).contains(&bb.y1));
                assert!(bb.x1 >= bb.x0 && bb.y1 >= bb.y0);
            }
        }
    }

    #[test]
    fn text_output_uses_element_text() {
        let resp = normalize(&fixture(), OutputFormat::Text, "document-parse");
        assert_eq!(resp.pages[0].blocks[0].content, "Hello LiteOCR");
    }

    #[test]
    fn falls_back_to_document_content_without_elements() {
        let result = DigitizationResult {
            content: Some(Content {
                markdown: Some("# Only content".into()),
                text: Some("Only content".into()),
                html: None,
            }),
            usage: Some(UsageWire { pages: 3, ..Default::default() }),
            ..Default::default()
        };
        let resp = normalize(&result, OutputFormat::Markdown, "document-parse");
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.usage.pages, 3);
        assert_eq!(resp.markdown, "# Only content");
    }

    #[test]
    fn maps_every_documented_category() {
        for (c, expected) in [
            ("paragraph", BlockType::Text),
            ("heading1", BlockType::Title),
            ("table", BlockType::Table),
            ("figure", BlockType::Figure),
            ("chart", BlockType::Figure),
            ("caption", BlockType::Caption),
            ("list", BlockType::List),
            ("header", BlockType::Header),
            ("footer", BlockType::Footer),
            ("footnote", BlockType::Footnote),
            ("equation", BlockType::Formula),
            ("code", BlockType::Text),
            ("index", BlockType::Other),
            ("something_new", BlockType::Other),
        ] {
            assert_eq!(map_category(c), expected, "category {c}");
        }
    }

    #[test]
    fn form_fields_defaults_and_overrides() {
        let req = DocumentRequest::from_path("a.pdf").provider_options(json!({
            "ocr": "force",
            "base64_encoding": ["table"],
            "chart_recognition": false,
            "async": true,
        }));
        let f = form_fields(&req, "document-parse").unwrap();
        let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("model"), Some("document-parse"));
        assert_eq!(get("output_formats"), Some(r#"["html","markdown","text"]"#));
        assert_eq!(get("coordinates"), Some("true"));
        assert_eq!(get("ocr"), Some("force"), "provider_options must win over the default");
        assert_eq!(f.iter().filter(|(k, _)| k == "ocr").count(), 1);
        assert_eq!(get("base64_encoding"), Some(r#"["table"]"#));
        assert_eq!(get("chart_recognition"), Some("false"));
        assert_eq!(get("async"), None, "`async` selects the endpoint, it is not a form field");
        assert!(form_fields(&req, "document-parse-nightly").is_ok());
        assert!(form_fields(&req, "ocr").is_err());
    }

    #[test]
    fn page_selection_is_applied_client_side() {
        let resp = normalize(&fixture(), OutputFormat::Markdown, "document-parse");
        let filtered = filter_pages(resp, &[(2, Some(2))]);
        assert_eq!(filtered.pages.len(), 1);
        assert_eq!(filtered.pages[0].page_number, 2);
        assert_eq!(filtered.usage.pages, 2, "billing still covers the whole document");
        assert!(filtered.markdown.contains("| Widget |"));
        assert!(!filtered.markdown.contains("Hello LiteOCR"));
    }

    #[test]
    fn error_body_is_classified() {
        let e = Error::from_http(
            NAME,
            401,
            r#"{"error":{"message":"Invalid API key","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        );
        assert_eq!(e.kind, crate::error::ErrorKind::Authentication);
        assert_eq!(e.message, "Invalid API key");
        let e = Error::from_http(NAME, 429, r#"{"error":{"message":"Too many requests","type":"rate_limit"}}"#);
        assert_eq!(e.kind, crate::error::ErrorKind::RateLimit);
    }

    /// Live smoke test. Needs `UPSTAGE_API_KEY`; run with
    /// `cargo test -p puffinparse-core upstage -- --ignored`.
    #[tokio::test]
    #[ignore = "needs UPSTAGE_API_KEY and network"]
    async fn upstage_live() {
        if std::env::var(ENV_KEY).map(|v| v.is_empty()).unwrap_or(true) {
            eprintln!("skipping: {ENV_KEY} not set");
            return;
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let req = DocumentRequest::from_path(path).timeout_secs(240.0);
        let resp = Upstage.parse(&req, "document-parse").await.expect("parse succeeds");
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert!(!resp.markdown.trim().is_empty());
        assert!(resp.pages.iter().all(|p| !p.blocks.is_empty()));
    }
}
