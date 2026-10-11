//! Google Cloud Document AI (`projects.locations.processors.process`).
//!
//! One synchronous call:
//! `POST https://{location}-documentai.googleapis.com/v1/projects/{project}/locations/{location}/processors/{processorId}:process`
//! with `{"rawDocument": {"content": <base64>, "mimeType": …}, "skipHumanReview": true}` and an
//! OAuth 2.0 bearer token.
//!
//! The processor id always comes from configuration (`GOOGLE_DOCUMENTAI_PROCESSOR_ID` or
//! `provider_options.processor_id`); the PuffinParse model name only says how to read the response:
//!
//! | model | processor | how the response is read |
//! |---|---|---|
//! | `ocr` | Document OCR | native `ocr` from `pages[].lines/tokens`; `parse` from `pages[].paragraphs` |
//! | `layout` | Layout Parser | `parse` from `documentLayout.blocks` (headings, tables, lists) |
//! | `form` | Form Parser | `parse` from paragraphs + tables; `extract` from `entities[]` |
//! | `prebuilt` | Invoice / W2 / Custom Extractor / … | `extract` from `entities[]` |
//!
//! Service-account → access-token exchange is **out of scope**: pass a token from
//! `gcloud auth print-access-token` (or any other minting path) in
//! `GOOGLE_DOCUMENTAI_ACCESS_TOKEN` / `provider_options.access_token` / `api_key`.
//!
//! Docs: <https://cloud.google.com/document-ai/docs/reference/rest/v1/projects.locations.processors/process>

use crate::error::{Error, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
use crate::types::{
    pages_from_blocks, BBox, Block, BlockType, Citation, DocumentInput, DocumentRequest, ExtractRequest,
    ExtractResponse, FieldInfo, Line, OutputFormat, Page, ParseResponse, TextPage, TextResponse, Usage, Word,
};
use crate::util::deep_merge;
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const NAME: &str = "google_documentai";
const ENV_KEY: &str = "GOOGLE_DOCUMENTAI_ACCESS_TOKEN";
const ENV_PROJECT: &str = "GOOGLE_DOCUMENTAI_PROJECT";
const ENV_LOCATION: &str = "GOOGLE_DOCUMENTAI_LOCATION";
const ENV_PROCESSOR: &str = "GOOGLE_DOCUMENTAI_PROCESSOR_ID";
const ENV_BASE: &str = "GOOGLE_DOCUMENTAI_BASE_URL";
const DEFAULT_LOCATION: &str = "us";

/// Google Cloud Document AI.
#[derive(Debug, Default, Clone, Copy)]
pub struct GoogleDocumentAi;

#[async_trait]
impl Provider for GoogleDocumentAi {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn parse(&self, request: &DocumentRequest, model: &str) -> Result<ParseResponse> {
        let (doc, raw) = process(request, model).await?;
        let mut resp = normalize_parse(&doc, request.output, model);
        resp.raw = raw;
        Ok(resp)
    }

    async fn ocr(&self, request: &DocumentRequest, model: &str) -> Result<TextResponse> {
        check_model(model)?;
        // The Layout Parser returns no tokens or lines, so its text comes from the parsed blocks.
        if model == "layout" {
            return Ok(TextResponse::from_parse(&self.parse(request, model).await?));
        }
        let (doc, raw) = process(request, model).await?;
        let mut resp = normalize_ocr(&doc, model);
        resp.raw = raw;
        Ok(resp)
    }

    async fn extract(&self, request: &ExtractRequest, model: &str) -> Result<ExtractResponse> {
        let (doc, raw) = process(&request.document, model).await?;
        let mut resp = normalize_extract(&doc, model);
        if doc.entities.is_empty() {
            tracing::warn!(
                "google_documentai: processor returned no entities — is the configured processor an \
                 extractor (form parser, prebuilt or custom extractor)?"
            );
        }
        // Document AI extracts the schema its processor was trained on; the request schema only
        // documents what the caller wanted.
        resp.metadata.insert("google_documentai_schema_source".into(), json!("processor"));
        resp.raw = raw;
        Ok(resp)
    }
}

/// One `:process` call. Returns the parsed `document` plus the untouched payload for `raw`.
async fn process(request: &DocumentRequest, model: &str) -> Result<(WireDocument, Option<Value>)> {
    check_model(model)?;
    let token = resolve_token(request)?;
    let cfg = Config::resolve(request)?;
    let base = provider::resolve_base_url(request, ENV_BASE, &cfg.default_base());
    let deadline = Deadline::new(request.timeout_secs);
    let retry = Retry::new(request.max_retries);
    let client = http::client();

    // Document AI reads bytes (`rawDocument`) or a GCS object, never an http(s) URL.
    let data = match provider::load_bytes(&request.input).await? {
        Some(bytes) => bytes,
        None => {
            let DocumentInput::Url { url } = &request.input else { unreachable!() };
            download(url, &deadline, retry).await?
        }
    };
    let body = build_body(request, model, &data)?;
    let url = format!("{base}/v1/{}:process", cfg.resource());
    tracing::debug!(processor = %cfg.resource(), "google_documentai: processing");

    let value: Value = http::with_retry(NAME, retry, &deadline, || {
        let rb = client.post(&url).bearer_auth(&token).timeout(deadline.request_timeout()).json(&body);
        async move { http::read_json(NAME, rb.send().await?).await }
    })
    .await?;

    let wire: ProcessResponse = serde_json::from_value(value.clone())
        .map_err(|e| Error::provider(format!("unexpected response shape: {e}")).with_provider(NAME))?;
    let raw = if request.include_raw { Some(value) } else { None };
    let doc = wire.document.ok_or_else(|| Error::provider("response has no document").with_provider(NAME))?;
    if let Some(err) = doc.error.as_ref().filter(|e| e.code.unwrap_or(0) != 0) {
        let msg = err.message.clone().unwrap_or_else(|| "document processing failed".into());
        return Err(Error::provider(msg).with_provider(NAME));
    }
    Ok((doc, raw))
}

fn check_model(model: &str) -> Result<()> {
    match model {
        "ocr" | "layout" | "form" | "prebuilt" => Ok(()),
        other => Err(Error::unsupported_model(format!("google_documentai: unknown model '{other}'"))),
    }
}

/// Project / location / processor, from `provider_options` first, then the environment.
#[derive(Debug, Clone)]
struct Config {
    project: String,
    location: String,
    processor_id: String,
    processor_version: Option<String>,
}

impl Config {
    fn resolve(request: &DocumentRequest) -> Result<Self> {
        let opt = |key: &str| request.option(key).and_then(Value::as_str).map(str::to_string);
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        let need = |value: Option<String>, key: &str, env_var: &str| {
            value.filter(|v| !v.trim().is_empty()).ok_or_else(|| {
                Error::input(format!("google_documentai: no {key}; set {env_var} or provider_options.{key}"))
                    .with_provider(NAME)
            })
        };
        let location = opt("location")
            .or_else(|| env(ENV_LOCATION))
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_LOCATION.to_string())
            .to_ascii_lowercase();
        // `location` becomes part of the hostname the access token is sent to.
        provider::host_label(NAME, "location", &location)?;
        Ok(Self {
            project: need(opt("project").or_else(|| env(ENV_PROJECT)), "project", ENV_PROJECT)?,
            location,
            processor_id: need(opt("processor_id").or_else(|| env(ENV_PROCESSOR)), "processor_id", ENV_PROCESSOR)?,
            processor_version: opt("processor_version"),
        })
    }

    fn default_base(&self) -> String {
        format!("https://{}-documentai.googleapis.com", self.location)
    }

    /// `projects/{p}/locations/{l}/processors/{id}[/processorVersions/{v}]`.
    fn resource(&self) -> String {
        let base = format!("projects/{}/locations/{}/processors/{}", self.project, self.location, self.processor_id);
        match &self.processor_version {
            Some(v) if !v.is_empty() => format!("{base}/processorVersions/{v}"),
            _ => base,
        }
    }
}

/// OAuth 2.0 access token: `api_key` on the request, `provider_options.access_token`, then the env var.
fn resolve_token(request: &DocumentRequest) -> Result<String> {
    if let Some(k) = request.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
        return Ok(k.to_string());
    }
    if let Some(k) = request.option("access_token").and_then(Value::as_str).filter(|k| !k.trim().is_empty()) {
        return Ok(k.to_string());
    }
    std::env::var(ENV_KEY).ok().filter(|k| !k.trim().is_empty()).ok_or_else(|| {
        Error::authentication(format!(
            "no access token for {NAME}: set {ENV_KEY} (e.g. `export {ENV_KEY}=$(gcloud auth print-access-token)`), \
             provider_options.access_token, or api_key"
        ))
        .with_provider(NAME)
    })
}

fn build_body(request: &DocumentRequest, model: &str, data: &bytes::Bytes) -> Result<Value> {
    let mut body = json!({
        "rawDocument": { "content": base64_encode(data), "mimeType": request.input.mime_type() },
        "skipHumanReview": true,
    });
    if let Some(spec) = &request.pages {
        let mut pages: Vec<u32> = Vec::new();
        for (start, end) in crate::util::parse_page_ranges(spec)? {
            let Some(end) = end else {
                return Err(Error::input(format!(
                    "google_documentai: open-ended page range '{spec}' is not supported; \
                     individualPageSelector needs explicit page numbers (e.g. \"10-15\")"
                ))
                .with_provider(NAME));
            };
            pages.extend(start..=end);
        }
        pages.sort_unstable();
        pages.dedup();
        body["processOptions"]["individualPageSelector"]["pages"] = json!(pages);
    }
    if let Some(lang) = &request.language {
        // `ocrConfig` is only accepted by OCR_PROCESSOR and FORM_PARSER_PROCESSOR.
        if matches!(model, "ocr" | "form") {
            body["processOptions"]["ocrConfig"]["hints"]["languageHints"] = json!([lang]);
        }
    }
    if let Some(opts) = &request.provider_options {
        let mut patch = opts.clone();
        if let Value::Object(o) = &mut patch {
            for key in ["project", "location", "processor_id", "processor_version", "access_token"] {
                o.remove(key);
            }
        }
        deep_merge(&mut body, &patch);
    }
    Ok(body)
}

/// Fetch a URL input so it can be sent as `rawDocument`.
async fn download(url: &str, deadline: &Deadline, retry: Retry) -> Result<bytes::Bytes> {
    Ok(crate::fetch::fetch_document(NAME, url, deadline, retry).await?.data)
}

/// Standard base64 with padding (RFC 4648 §4) — `rawDocument.content` is a base64 string.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

// ---- wire types -------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ProcessResponse {
    #[serde(default)]
    document: Option<WireDocument>,
}

/// `int64` fields come over the JSON wire as strings; page indices sometimes as numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Index(pub u64);

impl<'de> Deserialize<'de> for Index {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Index(match Value::deserialize(d)? {
            Value::String(s) => s.trim().parse::<u64>().map_err(serde::de::Error::custom)?,
            Value::Number(n) => n.as_u64().unwrap_or(0),
            Value::Null => 0,
            other => return Err(serde::de::Error::custom(format!("expected an integer, got {other}"))),
        }))
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct WireDocument {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub pages: Vec<WirePage>,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(rename = "documentLayout", default)]
    pub document_layout: Option<DocumentLayout>,
    #[serde(default)]
    pub error: Option<StatusWire>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct StatusWire {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct WirePage {
    #[serde(rename = "pageNumber", default)]
    pub page_number: u32,
    #[serde(default)]
    pub dimension: Option<Dimension>,
    #[serde(default)]
    pub layout: Option<Layout>,
    #[serde(default)]
    pub blocks: Vec<LayoutHolder>,
    #[serde(default)]
    pub paragraphs: Vec<LayoutHolder>,
    #[serde(default)]
    pub lines: Vec<LayoutHolder>,
    #[serde(default)]
    pub tokens: Vec<LayoutHolder>,
    #[serde(default)]
    pub tables: Vec<TableWire>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct Dimension {
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

/// `blocks`, `paragraphs`, `lines`, `tokens` and table cells all wrap a `layout`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct LayoutHolder {
    #[serde(default)]
    pub layout: Option<Layout>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Layout {
    #[serde(rename = "textAnchor", default)]
    pub text_anchor: Option<TextAnchor>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(rename = "boundingPoly", default)]
    pub bounding_poly: Option<BoundingPoly>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct TextAnchor {
    #[serde(rename = "textSegments", default)]
    pub text_segments: Vec<TextSegment>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct TextSegment {
    #[serde(rename = "startIndex", default)]
    pub start_index: Index,
    #[serde(rename = "endIndex", default)]
    pub end_index: Index,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct BoundingPoly {
    #[serde(default)]
    pub vertices: Vec<Vertex>,
    #[serde(rename = "normalizedVertices", default)]
    pub normalized_vertices: Vec<Vertex>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct Vertex {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct TableWire {
    #[serde(default)]
    pub layout: Option<Layout>,
    #[serde(rename = "headerRows", default)]
    pub header_rows: Vec<TableRow>,
    #[serde(rename = "bodyRows", default)]
    pub body_rows: Vec<TableRow>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct TableRow {
    #[serde(default)]
    pub cells: Vec<LayoutHolder>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Entity {
    #[serde(rename = "type", default)]
    pub entity_type: String,
    #[serde(rename = "mentionText", default)]
    pub mention_text: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(rename = "normalizedValue", default)]
    pub normalized_value: Option<Map<String, Value>>,
    #[serde(rename = "pageAnchor", default)]
    pub page_anchor: Option<PageAnchor>,
    #[serde(default)]
    pub properties: Vec<Entity>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct PageAnchor {
    #[serde(rename = "pageRefs", default)]
    pub page_refs: Vec<PageRef>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct PageRef {
    /// **Index into `document.pages`** (0-based), omitted when 0.
    #[serde(default)]
    pub page: Index,
    #[serde(rename = "boundingPoly", default)]
    pub bounding_poly: Option<BoundingPoly>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DocumentLayout {
    #[serde(default)]
    pub blocks: Vec<LayoutBlock>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct LayoutBlock {
    #[serde(rename = "blockId", default)]
    pub block_id: Option<String>,
    #[serde(rename = "pageSpan", default)]
    pub page_span: Option<PageSpan>,
    #[serde(rename = "boundingBox", default)]
    pub bounding_box: Option<BoundingPoly>,
    #[serde(rename = "textBlock", default)]
    pub text_block: Option<TextBlock>,
    #[serde(rename = "tableBlock", default)]
    pub table_block: Option<TableBlock>,
    #[serde(rename = "listBlock", default)]
    pub list_block: Option<ListBlock>,
    #[serde(rename = "imageBlock", default)]
    pub image_block: Option<ImageBlock>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct PageSpan {
    #[serde(rename = "pageStart", default)]
    pub page_start: u32,
    #[serde(rename = "pageEnd", default)]
    pub page_end: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct TextBlock {
    #[serde(default)]
    pub text: String,
    /// `paragraph`, `subtitle`, `heading-1` … `heading-5`, `header`, `footer`.
    #[serde(rename = "type", default)]
    pub block_type: String,
    #[serde(default)]
    pub blocks: Vec<LayoutBlock>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct TableBlock {
    #[serde(rename = "headerRows", default)]
    pub header_rows: Vec<LayoutTableRow>,
    #[serde(rename = "bodyRows", default)]
    pub body_rows: Vec<LayoutTableRow>,
    #[serde(default)]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct LayoutTableRow {
    #[serde(default)]
    pub cells: Vec<LayoutTableCell>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct LayoutTableCell {
    #[serde(default)]
    pub blocks: Vec<LayoutBlock>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct ListBlock {
    #[serde(rename = "listEntries", default)]
    pub list_entries: Vec<ListEntry>,
    /// `ordered` | `unordered`.
    #[serde(rename = "type", default)]
    pub list_type: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct ListEntry {
    #[serde(default)]
    pub blocks: Vec<LayoutBlock>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct ImageBlock {
    #[serde(rename = "imageText", default)]
    pub image_text: Option<String>,
}

// ---- normalisation -----------------------------------------------------------------------------

/// Text referenced by a `textAnchor`. Segment indices are UTF-8 offsets into `document.text`;
/// they fall back to character offsets when they are not byte boundaries.
fn anchor_text(text: &str, anchor: Option<&TextAnchor>) -> String {
    let Some(anchor) = anchor else { return String::new() };
    if anchor.text_segments.is_empty() {
        return anchor.content.clone().unwrap_or_default();
    }
    let mut out = String::new();
    for seg in &anchor.text_segments {
        let (start, end) = (seg.start_index.0 as usize, seg.end_index.0 as usize);
        if end <= start {
            continue;
        }
        if end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) {
            out.push_str(&text[start..end]);
        } else {
            out.extend(text.chars().skip(start).take(end - start));
        }
    }
    out
}

fn bbox_from_poly(poly: Option<&BoundingPoly>, dims: Option<Dimension>) -> Option<BBox> {
    let poly = poly?;
    let (points, normalized) = if !poly.normalized_vertices.is_empty() {
        (&poly.normalized_vertices, true)
    } else if !poly.vertices.is_empty() {
        (&poly.vertices, false)
    } else {
        return None;
    };
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    if normalized {
        Some(BBox { x0: x0.clamp(0.0, 1.0), y0: y0.clamp(0.0, 1.0), x1: x1.clamp(0.0, 1.0), y1: y1.clamp(0.0, 1.0) })
    } else {
        // Absolute pixels: only usable with the page dimensions.
        let d = dims.filter(|d| d.width > 0.0 && d.height > 0.0)?;
        BBox::from_xywh(x0, y0, x1 - x0, y1 - y0, d.width, d.height)
    }
}

fn page_dims(page: &WirePage) -> Option<Dimension> {
    page.dimension.filter(|d| d.width > 0.0 && d.height > 0.0)
}

fn page_number(page: &WirePage, index: usize) -> u32 {
    if page.page_number > 0 {
        page.page_number
    } else {
        index as u32 + 1
    }
}

pub(crate) fn normalize_parse(doc: &WireDocument, fmt: OutputFormat, model: &str) -> ParseResponse {
    let use_layout = model == "layout"
        || (doc.pages.is_empty() && doc.document_layout.as_ref().is_some_and(|l| !l.blocks.is_empty()));
    let (blocks, dims) = if use_layout { (layout_blocks(doc), BTreeMap::new()) } else { page_blocks(doc) };
    let blocks = match fmt {
        OutputFormat::Markdown => blocks,
        OutputFormat::Text => blocks
            .into_iter()
            .map(|mut b| {
                b.content = b.text.clone().unwrap_or_else(|| crate::types::markdown_to_text(&b.content));
                b
            })
            .collect(),
    };
    let pages = pages_from_blocks(blocks, &dims);
    let billed = billed_pages(doc, &pages);
    let usage = Usage { pages: billed, credits: None, provider_cost_usd: None };
    ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage)
}

fn billed_pages(doc: &WireDocument, pages: &[Page]) -> u32 {
    if !doc.pages.is_empty() {
        return doc.pages.len() as u32;
    }
    let spanned = doc
        .document_layout
        .as_ref()
        .map(|l| l.blocks.iter().filter_map(|b| b.page_span.map(|s| s.page_end.max(s.page_start))).max().unwrap_or(0))
        .unwrap_or(0);
    spanned.max(pages.len() as u32)
}

/// Document OCR / Form Parser: paragraphs (and tables, when the processor detects them).
fn page_blocks(doc: &WireDocument) -> (Vec<Block>, BTreeMap<u32, (f64, f64)>) {
    let mut blocks = Vec::new();
    let mut dims = BTreeMap::new();
    for (i, page) in doc.pages.iter().enumerate() {
        let number = page_number(page, i);
        let d = page_dims(page);
        if let Some(d) = d {
            dims.insert(number, (d.width, d.height));
        }
        let tables: Vec<(Option<BBox>, String)> = page
            .tables
            .iter()
            .map(|t| {
                (
                    bbox_from_poly(t.layout.as_ref().and_then(|l| l.bounding_poly.as_ref()), d),
                    table_markdown(t, &doc.text),
                )
            })
            .collect();
        for (bbox, markdown) in &tables {
            if markdown.trim().is_empty() {
                continue;
            }
            blocks.push(Block {
                block_type: BlockType::Table,
                content: markdown.clone(),
                text: Some(crate::types::markdown_to_text(markdown)),
                bbox: *bbox,
                confidence: None,
                page_number: number,
            });
        }
        for para in &page.paragraphs {
            let Some(layout) = &para.layout else { continue };
            let text = anchor_text(&doc.text, layout.text_anchor.as_ref());
            if text.trim().is_empty() {
                continue;
            }
            let bbox = bbox_from_poly(layout.bounding_poly.as_ref(), d);
            // Paragraphs inside a detected table are already covered by the table block.
            if let Some(bb) = bbox {
                if tables.iter().any(|(tb, _)| tb.is_some_and(|tb| contains_center(tb, bb))) {
                    continue;
                }
            }
            blocks.push(Block {
                block_type: BlockType::Text,
                content: text.trim().to_string(),
                text: Some(text.trim().to_string()),
                bbox,
                confidence: layout.confidence,
                page_number: number,
            });
        }
    }
    (blocks, dims)
}

fn contains_center(outer: BBox, inner: BBox) -> bool {
    let cx = (inner.x0 + inner.x1) / 2.0;
    let cy = (inner.y0 + inner.y1) / 2.0;
    cx >= outer.x0 && cx <= outer.x1 && cy >= outer.y0 && cy <= outer.y1
}

fn table_markdown(table: &TableWire, text: &str) -> String {
    let cell = |c: &LayoutHolder| {
        let raw = anchor_text(text, c.layout.as_ref().and_then(|l| l.text_anchor.as_ref()));
        raw.replace(['\n', '\r'], " ").replace('|', "\\|").trim().to_string()
    };
    let row = |r: &TableRow| format!("| {} |", r.cells.iter().map(cell).collect::<Vec<_>>().join(" | "));
    let mut lines = Vec::new();
    let width = table.header_rows.iter().chain(table.body_rows.iter()).map(|r| r.cells.len()).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    if let Some(header) = table.header_rows.first() {
        lines.push(row(header));
    } else {
        lines.push(format!("| {} |", vec![""; width].join(" | ")));
    }
    lines.push(format!("| {} |", vec!["---"; width].join(" | ")));
    for r in table.header_rows.iter().skip(1).chain(table.body_rows.iter()) {
        lines.push(row(r));
    }
    lines.join("\n")
}

/// Layout Parser: `documentLayout.blocks`, flattened in reading order.
fn layout_blocks(doc: &WireDocument) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(layout) = &doc.document_layout {
        for block in &layout.blocks {
            flatten_layout_block(block, &mut out);
        }
    }
    out
}

fn flatten_layout_block(block: &LayoutBlock, out: &mut Vec<Block>) {
    let page = block.page_span.map(|s| s.page_start.max(1)).unwrap_or(1);
    let bbox = bbox_from_poly(block.bounding_box.as_ref(), None);
    if let Some(tb) = &block.text_block {
        let text = tb.text.trim().to_string();
        push_block(out, map_text_block_type(&tb.block_type), heading_markdown(&tb.block_type, &text), text, bbox, page);
        // A text block can nest further blocks (sections under a heading).
        for child in &tb.blocks {
            flatten_layout_block(child, out);
        }
    }
    if let Some(tab) = &block.table_block {
        let md = layout_table_markdown(tab);
        let text = crate::types::markdown_to_text(&md);
        push_block(out, BlockType::Table, md, text, bbox, page);
    }
    if let Some(lb) = &block.list_block {
        let md = layout_list_markdown(lb);
        let text = crate::types::markdown_to_text(&md);
        push_block(out, BlockType::List, md, text, bbox, page);
    }
    if let Some(ib) = &block.image_block {
        let text = ib.image_text.clone().unwrap_or_default().trim().to_string();
        push_block(out, BlockType::Figure, text.clone(), text, bbox, page);
    }
}

fn push_block(
    out: &mut Vec<Block>,
    block_type: BlockType,
    content: String,
    text: String,
    bbox: Option<BBox>,
    page: u32,
) {
    if content.trim().is_empty() {
        return;
    }
    out.push(Block { block_type, content, text: Some(text), bbox, confidence: None, page_number: page });
}

fn map_text_block_type(block_type: &str) -> BlockType {
    match block_type {
        "heading-1" => BlockType::Title,
        "subtitle" | "heading-2" | "heading-3" | "heading-4" | "heading-5" => BlockType::SectionHeader,
        "header" => BlockType::Header,
        "footer" => BlockType::Footer,
        _ => BlockType::Text,
    }
}

fn heading_markdown(block_type: &str, text: &str) -> String {
    let level = match block_type {
        "heading-1" => 1,
        "heading-2" | "subtitle" => 2,
        "heading-3" => 3,
        "heading-4" => 4,
        "heading-5" => 5,
        _ => 0,
    };
    if level == 0 || text.is_empty() {
        text.to_string()
    } else {
        format!("{} {text}", "#".repeat(level))
    }
}

fn layout_cell_text(cell: &LayoutTableCell) -> String {
    let mut parts = Vec::new();
    for b in &cell.blocks {
        if let Some(tb) = &b.text_block {
            parts.push(tb.text.trim().to_string());
        }
        if let Some(ib) = &b.image_block {
            parts.push(ib.image_text.clone().unwrap_or_default().trim().to_string());
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" ").replace(['\n', '\r'], " ").replace('|', "\\|")
}

fn layout_table_markdown(table: &TableBlock) -> String {
    let width = table.header_rows.iter().chain(table.body_rows.iter()).map(|r| r.cells.len()).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let row =
        |r: &LayoutTableRow| format!("| {} |", r.cells.iter().map(layout_cell_text).collect::<Vec<_>>().join(" | "));
    let mut lines = Vec::new();
    if let Some(c) = table.caption.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        lines.push(format!("{c}\n"));
    }
    match table.header_rows.first() {
        Some(h) => lines.push(row(h)),
        None => lines.push(format!("| {} |", vec![""; width].join(" | "))),
    }
    lines.push(format!("| {} |", vec!["---"; width].join(" | ")));
    for r in table.header_rows.iter().skip(1).chain(table.body_rows.iter()) {
        lines.push(row(r));
    }
    lines.join("\n")
}

fn layout_list_markdown(list: &ListBlock) -> String {
    let ordered = list.list_type == "ordered";
    let mut lines: Vec<String> = Vec::new();
    for entry in &list.list_entries {
        let text = entry
            .blocks
            .iter()
            .filter_map(|b| b.text_block.as_ref().map(|t| t.text.trim().to_string()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        lines.push(if ordered { format!("{}. {text}", lines.len() + 1) } else { format!("- {text}") });
    }
    lines.join("\n")
}

/// Native OCR: lines and tokens with their boxes, straight off `pages[]`.
pub(crate) fn normalize_ocr(doc: &WireDocument, model: &str) -> TextResponse {
    let pages: Vec<TextPage> = doc
        .pages
        .iter()
        .enumerate()
        .map(|(i, page)| {
            let d = page_dims(page);
            let lines: Vec<Line> = page
                .lines
                .iter()
                .filter_map(|l| l.layout.as_ref())
                .map(|layout| Line {
                    text: anchor_text(&doc.text, layout.text_anchor.as_ref()).trim_end().to_string(),
                    bbox: bbox_from_poly(layout.bounding_poly.as_ref(), d),
                    confidence: layout.confidence,
                })
                .filter(|l| !l.text.trim().is_empty())
                .collect();
            let words: Vec<Word> = page
                .tokens
                .iter()
                .filter_map(|t| t.layout.as_ref())
                .map(|layout| Word {
                    text: anchor_text(&doc.text, layout.text_anchor.as_ref()).trim().to_string(),
                    bbox: bbox_from_poly(layout.bounding_poly.as_ref(), d),
                    confidence: layout.confidence,
                })
                .filter(|w| !w.text.is_empty())
                .collect();
            let text = {
                let page_text = anchor_text(&doc.text, page.layout.as_ref().and_then(|l| l.text_anchor.as_ref()));
                if page_text.trim().is_empty() {
                    lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
                } else {
                    page_text.trim().to_string()
                }
            };
            TextPage {
                page_number: page_number(page, i),
                width: d.map(|d| d.width),
                height: d.map(|d| d.height),
                text,
                lines,
                words,
            }
        })
        .collect();
    let usage = Usage { pages: pages.len() as u32, credits: None, provider_cost_usd: None };
    TextResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage)
}

/// Entities → a data object keyed by entity `type`, with per-field confidence and citations.
pub(crate) fn normalize_extract(doc: &WireDocument, model: &str) -> ExtractResponse {
    let mut fields = BTreeMap::new();
    let data = entities_to_value(&doc.entities, "", &mut fields);
    let usage = Usage { pages: doc.pages.len().max(1) as u32, credits: None, provider_cost_usd: None };
    let mut resp = ExtractResponse::new(NAME, &format!("{NAME}/{model}"), data, usage);
    resp.fields = fields;
    resp
}

fn entities_to_value(entities: &[Entity], pointer: &str, fields: &mut BTreeMap<String, FieldInfo>) -> Value {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for e in entities {
        *counts.entry(e.entity_type.as_str()).or_default() += 1;
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Map::new();
    for e in entities {
        let key = if e.entity_type.is_empty() { "unknown" } else { e.entity_type.as_str() };
        let repeated = counts.get(key).copied().unwrap_or(1) > 1;
        let idx = seen.entry(key).or_default();
        let child_pointer = if repeated {
            format!("{pointer}/{}/{}", escape_pointer(key), idx)
        } else {
            format!("{pointer}/{}", escape_pointer(key))
        };
        *idx += 1;
        let value = if e.properties.is_empty() {
            entity_value(e)
        } else {
            entities_to_value(&e.properties, &child_pointer, fields)
        };
        let citations: Vec<Citation> = e
            .page_anchor
            .as_ref()
            .map(|a| {
                a.page_refs
                    .iter()
                    .map(|r| Citation {
                        page_number: r.page.0 as u32 + 1,
                        bbox: bbox_from_poly(r.bounding_poly.as_ref(), None),
                        text: e.mention_text.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if e.confidence.is_some() || !citations.is_empty() {
            fields.insert(child_pointer, FieldInfo { confidence: e.confidence, citations });
        }
        if repeated {
            match out.entry(key.to_string()).or_insert_with(|| Value::Array(Vec::new())) {
                Value::Array(a) => a.push(value),
                slot => *slot = json!([value]),
            }
        } else {
            out.insert(key.to_string(), value);
        }
    }
    Value::Object(out)
}

fn entity_value(e: &Entity) -> Value {
    if let Some(nv) = &e.normalized_value {
        for key in ["booleanValue", "integerValue", "floatValue", "signatureValue"] {
            if let Some(v) = nv.get(key) {
                return v.clone();
            }
        }
        for key in ["moneyValue", "dateValue", "datetimeValue", "addressValue"] {
            if let Some(v) = nv.get(key) {
                // Keep the structured form, and the normalised text alongside it when present.
                if let (Some(text), Value::Object(o)) = (nv.get("text"), v) {
                    let mut o = o.clone();
                    o.entry("text".to_string()).or_insert_with(|| text.clone());
                    return Value::Object(o);
                }
                return v.clone();
            }
        }
        if let Some(Value::String(t)) = nv.get("text") {
            if !t.is_empty() {
                return json!(t);
            }
        }
    }
    json!(e.mention_text.clone().unwrap_or_default())
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ocr_doc() -> WireDocument {
        let raw = include_str!("../../tests/fixtures/google_documentai_ocr.json");
        let v: Value = serde_json::from_str(raw).unwrap();
        serde_json::from_value::<ProcessResponse>(v).unwrap().document.unwrap()
    }

    fn layout_doc() -> WireDocument {
        let raw = include_str!("../../tests/fixtures/google_documentai_layout.json");
        let v: Value = serde_json::from_str(raw).unwrap();
        serde_json::from_value::<ProcessResponse>(v).unwrap().document.unwrap()
    }

    fn form_doc() -> WireDocument {
        let raw = include_str!("../../tests/fixtures/google_documentai_form.json");
        let v: Value = serde_json::from_str(raw).unwrap();
        serde_json::from_value::<ProcessResponse>(v).unwrap().document.unwrap()
    }

    #[test]
    fn ocr_processor_parses_paragraphs() {
        let resp = normalize_parse(&ocr_doc(), OutputFormat::Markdown, "ocr");
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.provider, NAME);
        assert_eq!(resp.pages[0].width, Some(612.0));
        assert_eq!(resp.pages[0].blocks[0].block_type, BlockType::Text);
        assert_eq!(resp.pages[0].blocks[0].content, "Hello LiteOCR");
        assert_eq!(resp.pages[0].blocks[0].confidence, Some(0.987));
        assert!(resp.markdown.starts_with("Hello LiteOCR"));
        assert!(resp.text.contains("Invoice #1234"));
        let bb = resp.pages[0].blocks[0].bbox.unwrap();
        assert!((bb.x0 - 0.1).abs() < 1e-9 && (bb.y1 - 0.12).abs() < 1e-9, "{bb:?}");
    }

    #[test]
    fn ocr_mode_is_native() {
        let resp = normalize_ocr(&ocr_doc(), "ocr");
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.pages[0].lines.len(), 2);
        assert_eq!(resp.pages[0].lines[0].text, "Hello LiteOCR");
        assert_eq!(resp.pages[0].lines[0].confidence, Some(0.99));
        assert!(resp.pages[0].lines[0].bbox.is_some());
        assert_eq!(resp.pages[0].words.len(), 3);
        assert_eq!(resp.pages[0].words[0].text, "Hello");
        assert!(resp.pages[0].words[0].bbox.is_some(), "tokens keep their own boxes");
        assert!(resp.text.contains("Invoice #1234"));
    }

    #[test]
    fn text_segments_index_into_document_text() {
        let doc = ocr_doc();
        let anchor = TextAnchor {
            text_segments: vec![TextSegment { start_index: Index(0), end_index: Index(5) }],
            content: None,
        };
        assert_eq!(anchor_text(&doc.text, Some(&anchor)), "Hello");
        // String-encoded int64 indices are accepted.
        let seg: TextSegment = serde_json::from_str(r#"{"startIndex":"6","endIndex":"13"}"#).unwrap();
        assert_eq!(seg.start_index, Index(6));
        let anchor = TextAnchor { text_segments: vec![seg], content: None };
        assert_eq!(anchor_text(&doc.text, Some(&anchor)), "LiteOCR");
    }

    #[test]
    fn form_parser_tables_become_markdown_and_absorb_paragraphs() {
        let resp = normalize_parse(&form_doc(), OutputFormat::Markdown, "form");
        let table = resp.pages[0].blocks.iter().find(|b| b.block_type == BlockType::Table).expect("table block");
        assert_eq!(table.content, "| Item | Amount |\n| --- | --- |\n| Widget | $56.78 |");
        // The cell paragraphs sit inside the table box and must not be emitted twice.
        assert_eq!(resp.pages[0].blocks.iter().filter(|b| b.content.contains("Widget")).count(), 1);
        assert!(resp.pages[0].blocks.iter().any(|b| b.block_type == BlockType::Text));
    }

    #[test]
    fn layout_parser_blocks_become_headings_tables_and_lists() {
        let resp = normalize_parse(&layout_doc(), OutputFormat::Markdown, "layout");
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.pages[0].blocks[0].block_type, BlockType::Title);
        assert_eq!(resp.pages[0].blocks[0].content, "# Hello LiteOCR");
        assert_eq!(resp.pages[0].blocks[1].block_type, BlockType::SectionHeader);
        assert_eq!(resp.pages[0].blocks[1].content, "## Invoice");
        assert!(resp.pages[0].blocks.iter().any(|b| b.block_type == BlockType::List && b.content.contains("- Net 30")));
        let table = resp.pages[1].blocks.iter().find(|b| b.block_type == BlockType::Table).expect("table block");
        assert!(table.content.contains("| Item | Amount |"), "{}", table.content);
        assert!(table.content.contains("| Widget | $56.78 |"));
        let bb = resp.pages[0].blocks[0].bbox.unwrap();
        assert!((0.0..=1.0).contains(&bb.x0) && bb.y1 <= 1.0);
    }

    #[test]
    fn entities_become_data_with_citations() {
        let resp = normalize_extract(&form_doc(), "form");
        assert_eq!(resp.data["invoice_id"], "1234");
        assert_eq!(resp.data["total_amount"]["units"], "56");
        assert_eq!(resp.data["total_amount"]["text"], "56.78");
        // Repeated entity types collapse into an array.
        assert_eq!(resp.data["line_item"].as_array().unwrap().len(), 2);
        assert_eq!(resp.data["line_item"][0]["line_item/description"], "Widget");
        let f = resp.fields.get("/invoice_id").expect("field info");
        assert_eq!(f.confidence, Some(0.96));
        assert_eq!(f.citations[0].page_number, 1, "pageRefs are 0-based indices");
        assert!(f.citations[0].bbox.is_some());
        assert_eq!(resp.fields.get("/line_item/1").unwrap().citations[0].page_number, 2);
        assert_eq!(resp.usage.pages, 2);
    }

    #[test]
    fn body_carries_base64_page_selection_and_options() {
        let req = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"hello"), "a.pdf")
            .pages("1-2,5")
            .language("de")
            .provider_options(json!({
                "project": "p", "location": "eu", "processor_id": "abc",
                "processOptions": { "ocrConfig": { "enableNativePdfParsing": true } },
                "labels": { "team": "docs" }
            }));
        let body = build_body(&req, "ocr", &bytes::Bytes::from_static(b"hello")).unwrap();
        assert_eq!(body["rawDocument"]["content"], "aGVsbG8=");
        assert_eq!(body["rawDocument"]["mimeType"], "application/pdf");
        assert_eq!(body["skipHumanReview"], true);
        assert_eq!(body["processOptions"]["individualPageSelector"]["pages"], json!([1, 2, 5]));
        assert_eq!(body["processOptions"]["ocrConfig"]["hints"]["languageHints"], json!(["de"]));
        assert_eq!(body["processOptions"]["ocrConfig"]["enableNativePdfParsing"], true);
        assert_eq!(body["labels"]["team"], "docs");
        // Config keys are consumed locally, never sent.
        assert!(body.get("project").is_none() && body.get("processor_id").is_none());
        // The Layout Parser rejects ocrConfig, so PuffinParse adds no language hint for it
        // (an explicit provider_options passthrough is still the caller's business).
        let body = build_body(&req, "layout", &bytes::Bytes::from_static(b"hello")).unwrap();
        assert!(body["processOptions"]["ocrConfig"].get("hints").is_none());
        let plain = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"x"), "a.pdf").language("de");
        let body = build_body(&plain, "layout", &bytes::Bytes::from_static(b"x")).unwrap();
        assert!(body.get("processOptions").is_none());
        // Open-ended ranges cannot be expressed with individualPageSelector.
        let req = DocumentRequest::from_bytes(bytes::Bytes::from_static(b"x"), "a.pdf").pages("3-");
        assert_eq!(
            build_body(&req, "ocr", &bytes::Bytes::from_static(b"x")).unwrap_err().kind,
            crate::error::ErrorKind::Input
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0u8, 255, 17, 3]), "AP8RAw==");
    }

    #[test]
    fn config_and_endpoint_resolution() {
        let req = DocumentRequest::from_path("a.pdf")
            .provider_options(json!({"project": "my-proj", "location": "EU", "processor_id": "1a2b", "processor_version": "pretrained-ocr-v2.0"}));
        let cfg = Config::resolve(&req).unwrap();
        assert_eq!(cfg.default_base(), "https://eu-documentai.googleapis.com");
        assert_eq!(
            cfg.resource(),
            "projects/my-proj/locations/eu/processors/1a2b/processorVersions/pretrained-ocr-v2.0"
        );
        let req = DocumentRequest::from_path("a.pdf").provider_options(json!({"project": "p", "processor_id": "x"}));
        let cfg = Config::resolve(&req).unwrap();
        assert_eq!(cfg.location, "us", "default location");
        assert_eq!(cfg.resource(), "projects/p/locations/us/processors/x");
        // A missing processor id is a config error, before any network call.
        let req = DocumentRequest::from_path("a.pdf").provider_options(json!({"project": "p"}));
        let e = Config::resolve(&req).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Input);
        assert!(e.message.contains(ENV_PROCESSOR), "{e}");
        // The location is spliced into the hostname that receives the token: one DNS label only.
        for bad in ["evil.example/", "evil.example#", "eu.evil", "us@evil.example"] {
            let req = DocumentRequest::from_path("a.pdf")
                .provider_options(json!({"project": "p", "processor_id": "x", "location": bad}));
            let e = Config::resolve(&req).unwrap_err();
            assert_eq!(e.kind, crate::error::ErrorKind::Input, "{bad}");
        }
    }

    #[test]
    fn google_error_envelope_is_classified() {
        let body = r#"{"error":{"code":403,"message":"Permission 'documentai.processors.processOnline' denied","status":"PERMISSION_DENIED"}}"#;
        let e = Error::from_http(NAME, 403, body);
        assert_eq!(e.kind, crate::error::ErrorKind::Authentication);
        assert!(e.message.contains("documentai.processors.processOnline"), "{}", e.message);
        let body =
            r#"{"error":{"code":400,"message":"Document pages exceed the limit: 15","status":"INVALID_ARGUMENT"}}"#;
        let e = Error::from_http(NAME, 400, body);
        assert_eq!(e.kind, crate::error::ErrorKind::BadRequest);
        assert_eq!(e.message, "Document pages exceed the limit: 15");
        assert_eq!(Error::from_http(NAME, 429, "{}").kind, crate::error::ErrorKind::RateLimit);
    }

    #[test]
    fn unknown_models_are_rejected() {
        assert!(check_model("invoice").is_err());
        for m in ["ocr", "layout", "form", "prebuilt"] {
            assert!(check_model(m).is_ok());
        }
    }

    /// Live smoke test. Needs a token plus project / processor configuration; run with
    /// `cargo test -p puffinparse-core google_documentai -- --ignored`.
    #[tokio::test]
    #[ignore = "needs GOOGLE_DOCUMENTAI_ACCESS_TOKEN + _PROJECT + _PROCESSOR_ID and network"]
    async fn google_documentai_live() {
        for var in [ENV_KEY, ENV_PROJECT, ENV_PROCESSOR] {
            if std::env::var(var).map(|v| v.is_empty()).unwrap_or(true) {
                eprintln!("skipping: {var} not set");
                return;
            }
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let req = DocumentRequest::from_path(path).timeout_secs(240.0);
        let resp = GoogleDocumentAi.parse(&req, "ocr").await.expect("parse succeeds");
        assert_eq!(resp.usage.pages, 2);
        assert!(!resp.markdown.trim().is_empty());
        let text = GoogleDocumentAi.ocr(&req, "ocr").await.expect("ocr succeeds");
        assert!(text.pages.iter().all(|p| !p.words.is_empty()));
    }
}
