//! Unstructured (unstructured.io) Partition endpoint.
//!
//! Flow: one synchronous call — `POST /general/v0/general` (multipart `files` + `strategy`,
//! `coordinates`, `include_page_breaks`) → a flat JSON array of document elements.
//! There is no job to poll and no remote-URL input, so URL documents are downloaded and uploaded.

use crate::error::{Error, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
use crate::types::{BBox, Block, BlockType, DocumentInput, DocumentRequest, OutputFormat, ParseResponse, Usage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const NAME: &str = "unstructured";
const ENV_KEY: &str = "UNSTRUCTURED_API_KEY";
const ENV_BASE: &str = "UNSTRUCTURED_BASE_URL";
const DEFAULT_BASE: &str = "https://api.unstructuredapp.io";
const PARTITION_PATH: &str = "/general/v0/general";

#[derive(Debug, Default, Clone, Copy)]
pub struct Unstructured;

#[async_trait]
impl Provider for Unstructured {
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
        // `pages` has no server-side equivalent on this endpoint; elements are filtered afterwards.
        let keep = request.pages.as_deref().map(crate::util::parse_page_ranges).transpose()?;
        // The Partition endpoint only takes an uploaded file, so a URL input is fetched first.
        let data = load_or_download(request, &deadline).await?;

        let body: Value = http::with_retry(NAME, retry, &deadline, || {
            let mut form = reqwest::multipart::Form::new();
            for (k, v) in &fields {
                form = form.text(k.clone(), v.clone());
            }
            form = form.part("files", provider::file_part(data.clone(), &request.input));
            let rb = client
                .post(format!("{base}{PARTITION_PATH}"))
                .header("unstructured-api-key", &api_key)
                .header("accept", "application/json")
                .timeout(deadline.request_timeout())
                .multipart(form);
            async move { http::read_json(NAME, rb.send().await?).await }
        })
        .await?;
        let elements: Vec<Element> = serde_json::from_value(body.clone())?;
        tracing::debug!(elements = elements.len(), "unstructured: partitioned");

        // Keep the untouched payload for `include_raw`: our structs ignore fields they do not map.
        let raw = if request.include_raw { Some(body) } else { None };
        let mut resp = normalize(&elements, request.output, model, keep.as_deref());
        resp.raw = raw;
        Ok(resp)
    }
}

/// Read the document bytes, downloading them first when the input is a URL.
async fn load_or_download(request: &DocumentRequest, deadline: &Deadline) -> Result<bytes::Bytes> {
    if let Some(data) = provider::load_bytes(&request.input).await? {
        return Ok(data);
    }
    let DocumentInput::Url { url } = &request.input else { unreachable!("load_bytes only returns None for URLs") };
    // No URL input on this endpoint: download through the address-filtered fetch path.
    let retry = Retry::new(request.max_retries);
    Ok(crate::fetch::fetch_document(NAME, url, deadline, retry).await?.data)
}

/// Multipart text fields for the partition call.
fn form_fields(request: &DocumentRequest, model: &str) -> Result<Vec<(String, String)>> {
    let strategy = match model {
        "hi_res" | "fast" | "auto" => model,
        other => return Err(Error::unsupported_model(format!("unstructured: unknown strategy '{other}'"))),
    };
    let mut fields: Vec<(String, String)> = vec![
        ("strategy".into(), strategy.into()),
        ("output_format".into(), "application/json".into()),
        ("coordinates".into(), "true".into()),
        ("include_page_breaks".into(), "true".into()),
    ];
    if let Some(lang) = &request.language {
        // `languages` is a repeatable field of Tesseract language codes; a BCP-47 tag is passed through.
        fields.push(("languages".into(), lang.clone()));
    }
    if let Some(Value::Object(opts)) = &request.provider_options {
        for (k, v) in opts {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Null => continue,
                // Arrays of scalars become repeated fields (`languages`, `extract_image_block_types`).
                Value::Array(items) => {
                    fields.retain(|(key, _)| key != k);
                    for item in items {
                        let s = match item {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        fields.push((k.clone(), s));
                    }
                    continue;
                }
                other => other.to_string(),
            };
            // Later entries win for scalar fields; drop our default so provider options override it.
            fields.retain(|(key, _)| key != k);
            fields.push((k.clone(), s));
        }
    }
    Ok(fields)
}

// ---- wire types -------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Element {
    #[serde(rename = "type", default)]
    pub element_type: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub metadata: ElementMetadata,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ElementMetadata {
    #[serde(default)]
    pub page_number: Option<u32>,
    #[serde(default)]
    pub coordinates: Option<Coordinates>,
    #[serde(default)]
    pub text_as_html: Option<String>,
    #[serde(default)]
    pub detection_class_prob: Option<f64>,
    #[serde(default)]
    pub category_depth: Option<u32>,
    #[serde(default)]
    pub filetype: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Coordinates {
    #[serde(default)]
    pub points: Vec<[f64; 2]>,
    /// `"PixelSpace"` on the wire, but older payloads nest the dimensions in an object.
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub layout_width: Option<f64>,
    #[serde(default)]
    pub layout_height: Option<f64>,
}

impl Coordinates {
    /// Page dimensions the `points` are expressed in.
    fn dims(&self) -> Option<(f64, f64)> {
        let nested = |key: &str| {
            self.system.as_ref().and_then(|s| s.as_object()).and_then(|o| o.get(key)).and_then(Value::as_f64)
        };
        let w = self.layout_width.or_else(|| nested("layout_width"))?;
        let h = self.layout_height.or_else(|| nested("layout_height"))?;
        (w > 0.0 && h > 0.0).then_some((w, h))
    }

    /// Axis-aligned box around the polygon, normalised against the page dimensions.
    fn bbox(&self) -> Option<BBox> {
        let (w, h) = self.dims()?;
        polygon_bbox(&self.points, w, h)
    }
}

/// Smallest enclosing box of a polygon, normalised by page size. Shared with the Mathpix provider.
pub(crate) fn polygon_bbox(points: &[[f64; 2]], page_w: f64, page_h: f64) -> Option<BBox> {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in points {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    if x0 > x1 || y0 > y1 {
        return None;
    }
    BBox::from_xywh(x0, y0, x1 - x0, y1 - y0, page_w, page_h)
}

// ---- normalisation -----------------------------------------------------------------------------

/// Map an Unstructured element type onto the unified block vocabulary.
/// `Title` becomes a `title` only at the top of the hierarchy; deeper ones are section headers.
fn map_element_type(t: &str, depth: Option<u32>) -> BlockType {
    match t {
        "Title" => {
            if depth.unwrap_or(0) == 0 {
                BlockType::Title
            } else {
                BlockType::SectionHeader
            }
        }
        "Headline" | "Subtitle" | "SectionHeader" => BlockType::SectionHeader,
        "NarrativeText" | "UncategorizedText" | "Text" | "CompositeElement" | "Address" | "EmailAddress"
        | "CodeSnippet" | "FormKeysValues" | "Field-Name" | "Value" | "Abstract" | "Threading" => BlockType::Text,
        "ListItem" | "List-item" | "BulletedText" => BlockType::List,
        "Table" | "TableChunk" => BlockType::Table,
        "Image" | "Picture" | "Figure" => BlockType::Figure,
        "Header" | "PageHeader" => BlockType::Header,
        "Footer" | "PageFooter" => BlockType::Footer,
        "Footnote" => BlockType::Footnote,
        "FigureCaption" | "Caption" => BlockType::Caption,
        "Formula" => BlockType::Formula,
        _ => BlockType::Other,
    }
}

/// Render one element as markdown: headings get `#`s, list items a dash, tables their HTML table.
fn block_markdown(el: &Element, block_type: BlockType) -> String {
    let text = el.text.trim();
    match block_type {
        BlockType::Table => match el.metadata.text_as_html.as_deref() {
            Some(html) if !html.trim().is_empty() => {
                html_table_to_markdown(html).unwrap_or_else(|| html.trim().to_string())
            }
            _ => text.to_string(),
        },
        BlockType::Title => format!("# {text}"),
        BlockType::SectionHeader => {
            let level = el.metadata.category_depth.unwrap_or(1).clamp(1, 5) + 1;
            format!("{} {text}", "#".repeat(level as usize))
        }
        BlockType::List => format!("- {text}"),
        _ => text.to_string(),
    }
}

/// Plain-text form of an element (tables fall back to their `text`, which is already plain).
fn block_text(el: &Element) -> String {
    el.text.trim().to_string()
}

pub(crate) fn normalize(
    elements: &[Element],
    fmt: OutputFormat,
    model: &str,
    keep: Option<&[(u32, Option<u32>)]>,
) -> ParseResponse {
    let mut blocks: Vec<Block> = Vec::new();
    let mut page_dims: BTreeMap<u32, (f64, f64)> = BTreeMap::new();
    let mut seen_pages: BTreeSet<u32> = BTreeSet::new();
    // Files without page metadata (HTML, email, text) still get page breaks when the type supports
    // them; the counter keeps those documents on sensible 1-based page numbers.
    let mut implicit_page = 1u32;

    for el in elements {
        if el.element_type == "PageBreak" {
            implicit_page += 1;
            continue;
        }
        let page_number = el.metadata.page_number.unwrap_or(implicit_page).max(1);
        seen_pages.insert(page_number);
        if let Some(dims) = el.metadata.coordinates.as_ref().and_then(Coordinates::dims) {
            page_dims.entry(page_number).or_insert(dims);
        }
        if !page_selected(keep, page_number) {
            continue;
        }
        let block_type = map_element_type(&el.element_type, el.metadata.category_depth);
        let markdown = block_markdown(el, block_type);
        let text = block_text(el);
        let content = match fmt {
            OutputFormat::Markdown => markdown,
            OutputFormat::Text => text.clone(),
        };
        if content.trim().is_empty() {
            continue;
        }
        blocks.push(Block {
            block_type,
            content,
            text: Some(text),
            bbox: el.metadata.coordinates.as_ref().and_then(Coordinates::bbox),
            confidence: el.metadata.detection_class_prob,
            page_number,
        });
    }

    let pages = crate::types::pages_from_blocks(blocks, &page_dims);
    // Billed pages are everything the API processed, even when `pages` filtered the output.
    let billed = seen_pages.len().max(pages.len()) as u32;
    let usage = Usage { pages: billed.max(1), credits: None, provider_cost_usd: None };
    let mut resp = ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage);
    if keep.is_some() {
        resp.metadata.insert("unstructured_pages_filtered_client_side".into(), json!(true));
    }
    if let Some(ft) = elements.iter().find_map(|e| e.metadata.filetype.clone()) {
        resp.metadata.insert("unstructured_filetype".into(), json!(ft));
    }
    resp
}

fn page_selected(keep: Option<&[(u32, Option<u32>)]>, page: u32) -> bool {
    match keep {
        None => true,
        Some(ranges) => ranges.iter().any(|(s, e)| page >= *s && e.map(|e| page <= e).unwrap_or(true)),
    }
}

// ---- small HTML helpers (shared with the Datalab provider) --------------------------------------

/// Decode the handful of entities document APIs emit, and drop tags.
pub(crate) fn html_to_text(html: &str) -> String {
    let text = crate::types::strip_html_tags(html);
    let text = decode_entities(&text);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn decode_entities(s: &str) -> String {
    let mut out = s.to_string();
    for (from, to) in [("&nbsp;", " "), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'")] {
        out = out.replace(from, to);
    }
    // `&amp;` last so `&amp;lt;` does not become `<`.
    out.replace("&amp;", "&")
}

/// Convert a *simple* HTML table (no spans, no nesting, rectangular) to a markdown table.
/// Returns `None` when the table is not simple enough to survive the conversion; callers then keep
/// the HTML, which is valid markdown for consumers that render it.
pub(crate) fn html_table_to_markdown(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    if lower.matches("<table").count() > 1 {
        return None; // nested tables
    }
    if lower.contains("colspan") || lower.contains("rowspan") {
        return None;
    }
    let rows = parse_table_rows(html);
    if rows.len() < 2 {
        return None;
    }
    let width = rows[0].len();
    if width == 0 || rows.iter().any(|r| r.len() != width) {
        return None;
    }
    let esc = |c: &String| c.replace('|', "\\|");
    let mut out = String::new();
    out.push_str(&format!("| {} |\n", rows[0].iter().map(esc).collect::<Vec<_>>().join(" | ")));
    out.push_str(&format!("|{}\n", " --- |".repeat(width)));
    for row in &rows[1..] {
        out.push_str(&format!("| {} |\n", row.iter().map(esc).collect::<Vec<_>>().join(" | ")));
    }
    Some(out.trim_end().to_string())
}

/// Split table HTML into rows of cell texts. `<tr>` starts a row; a `<thead>` that holds bare
/// `<th>` cells (as Unstructured emits) is treated as one row, as is a run of cells before any
/// `<tr>`.
fn parse_table_rows(html: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let flush = |current: &mut Vec<String>, rows: &mut Vec<Vec<String>>| {
        if !current.is_empty() {
            rows.push(std::mem::take(current));
        }
    };
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(close) = html[i..].find('>').map(|p| i + p) else { break };
        let tag = html[i + 1..close].trim();
        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
        match name.as_str() {
            "tr" => flush(&mut current, &mut rows),
            "/thead" | "/tbody" | "/tfoot" | "/tr" | "/table" => flush(&mut current, &mut rows),
            "td" | "th" => {
                // Cell content runs until the next cell or row tag.
                let start = close + 1;
                let rest = &html[start..];
                let end = ["</td", "</th", "<td", "<th", "</tr", "<tr", "</table"]
                    .iter()
                    .filter_map(|t| find_ignore_case(rest, t))
                    .min()
                    .unwrap_or(rest.len());
                current.push(html_to_text(&rest[..end]));
                i = start + end;
                continue;
            }
            _ => {}
        }
        i = close + 1;
    }
    flush(&mut current, &mut rows);
    rows
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.to_ascii_lowercase();
    h.find(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<Element> {
        let raw = include_str!("../../tests/fixtures/unstructured_elements.json");
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn normalizes_fixture() {
        let resp = normalize(&fixture(), OutputFormat::Markdown, "hi_res", None);
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.pages[0].page_number, 1);
        assert_eq!(resp.pages[0].blocks[0].block_type, BlockType::Title);
        assert!(resp.pages[0].markdown.starts_with("# Hello LiteOCR"));
        // PageBreak elements never become blocks.
        assert!(resp.pages.iter().flat_map(|p| &p.blocks).all(|b| b.content != "<PAGE BREAK>"));
        let bb = resp.pages[0].blocks[0].bbox.unwrap();
        assert!(bb.x0 > 0.0 && bb.x1 < 1.0 && bb.y0 < bb.y1, "{bb:?}");
        assert!((bb.x0 - 0.1).abs() < 1e-6, "{bb:?}");
        assert_eq!(resp.pages[0].blocks[0].confidence, Some(0.92));
        assert_eq!(resp.pages[0].width, Some(1700.0));
        // The table arrives as HTML and is converted to a markdown table.
        let table = resp.pages[1].blocks.iter().find(|b| b.block_type == BlockType::Table).unwrap();
        assert!(table.content.starts_with("| Item | Qty | Price |"), "{}", table.content);
        assert!(table.content.contains("| Widget | 2 | $10.00 |"));
        assert_eq!(table.text.as_deref(), Some("Item Qty Price Widget 2 $10.00"));
        // Header/footer/list/section header mapping.
        let kinds: Vec<BlockType> = resp.pages[1].blocks.iter().map(|b| b.block_type).collect();
        assert!(kinds.contains(&BlockType::SectionHeader));
        assert!(kinds.contains(&BlockType::List));
        assert!(kinds.contains(&BlockType::Footer));
        assert!(resp.markdown.contains("## Line Items"));
        assert!(resp.text.contains("Widget"));
    }

    #[test]
    fn text_output_drops_markdown_syntax() {
        let resp = normalize(&fixture(), OutputFormat::Text, "fast", None);
        assert!(!resp.pages[0].markdown.starts_with('#'));
        assert!(resp.pages[0].text.starts_with("Hello LiteOCR"));
    }

    #[test]
    fn page_filter_keeps_billed_page_count() {
        let ranges = crate::util::parse_page_ranges("2").unwrap();
        let resp = normalize(&fixture(), OutputFormat::Markdown, "hi_res", Some(&ranges));
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.pages[0].page_number, 2);
        // Still billed for every page the API processed.
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.metadata["unstructured_pages_filtered_client_side"], json!(true));
    }

    #[test]
    fn elements_without_page_numbers_use_page_breaks() {
        let els: Vec<Element> = serde_json::from_str(
            r#"[{"type":"Title","text":"A","metadata":{}},
                {"type":"PageBreak","text":"","metadata":{}},
                {"type":"NarrativeText","text":"B","metadata":{}}]"#,
        )
        .unwrap();
        let resp = normalize(&els, OutputFormat::Markdown, "fast", None);
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.pages[1].text, "B");
        assert_eq!(resp.usage.pages, 2);
    }

    #[test]
    fn form_fields_pin_defaults_and_merge_options() {
        let req = DocumentRequest::from_url("https://x/y.pdf")
            .language("eng")
            .provider_options(json!({"strategy": "vlm", "vlm_model": "gpt-4o", "extract_image_block_types": ["Image", "Table"], "unique_element_ids": true}));
        let f = form_fields(&req, "hi_res").unwrap();
        let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("strategy"), Some("vlm"), "provider_options override the model's strategy");
        assert_eq!(f.iter().filter(|(k, _)| k == "strategy").count(), 1);
        assert_eq!(get("coordinates"), Some("true"));
        assert_eq!(get("include_page_breaks"), Some("true"));
        assert_eq!(get("output_format"), Some("application/json"));
        assert_eq!(get("languages"), Some("eng"));
        assert_eq!(get("unique_element_ids"), Some("true"));
        assert_eq!(f.iter().filter(|(k, _)| k == "extract_image_block_types").count(), 2);
        assert!(form_fields(&req, "ocr_only").is_err());
    }

    #[test]
    fn converts_simple_tables_only() {
        let simple = "<table><thead><th>A</th><th>B</th></thead><tr><td>1</td><td>2</td></tr></table>";
        assert_eq!(html_table_to_markdown(simple).unwrap(), "| A | B |\n| --- | --- |\n| 1 | 2 |");
        let with_tr = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
        assert_eq!(html_table_to_markdown(with_tr).unwrap(), "| A | B |\n| --- | --- |\n| 1 | 2 |");
        // Ragged, spanning or nested tables are kept as HTML.
        assert!(html_table_to_markdown("<table><tr><td>1</td></tr><tr><td>1</td><td>2</td></tr></table>").is_none());
        assert!(html_table_to_markdown("<table><tr><td colspan=\"2\">1</td></tr><tr><td>a</td></tr></table>").is_none());
        assert!(html_table_to_markdown("<table><tr><td><table><tr><td>x</td></tr></table></td></tr></table>").is_none());
        // Pipes inside cells are escaped, entities decoded.
        let piped = "<table><tr><th>a|b</th><th>c</th></tr><tr><td>Q&amp;A</td><td>&nbsp;</td></tr></table>";
        assert_eq!(html_table_to_markdown(piped).unwrap(), "| a\\|b | c |\n| --- | --- |\n| Q&A |  |");
    }

    #[test]
    fn html_to_text_strips_tags_and_entities() {
        assert_eq!(html_to_text("<p>a &amp; <b>b</b></p>"), "a & b");
        assert_eq!(html_to_text("x&nbsp;&nbsp;y"), "x y");
    }

    /// Live smoke test. Run with:
    /// `UNSTRUCTURED_API_KEY=… cargo test -p puffinparse-core unstructured -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs UNSTRUCTURED_API_KEY and network"]
    async fn live_parse() {
        if std::env::var(ENV_KEY).map(|v| v.trim().is_empty()).unwrap_or(true) {
            eprintln!("skipping: {ENV_KEY} not set");
            return;
        }
        let sample =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let req = DocumentRequest::from_path(sample).model("unstructured/fast").timeout_secs(240.0);
        let resp = Unstructured.parse(&req, "fast").await.expect("partition succeeds");
        assert!(!resp.markdown.trim().is_empty());
        assert!(resp.usage.pages >= 1);
        assert!(resp.pages.iter().any(|p| !p.blocks.is_empty()));
    }
}
