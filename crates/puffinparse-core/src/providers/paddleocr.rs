//! PaddleOCR 3.x through its self-hosted serving API (PaddleX "basic serving").
//!
//! Each served pipeline is its own HTTP service (`paddlex --serve --pipeline <name>` or
//! `paddleocr` serving), so the two modes can point at different servers:
//!
//! - `ocr` (native): `POST {PADDLEOCR_BASE_URL}/ocr` — the general OCR pipeline (PP-OCRv5).
//!   `result.ocrResults[i].prunedResult` carries `rec_texts`, `rec_scores` and `rec_boxes`
//!   (`[x_min, y_min, x_max, y_max]` pixels) or `rec_polys`; one element per page.
//! - `parse`: `POST {PADDLEOCR_PARSE_BASE_URL or PADDLEOCR_BASE_URL}/layout-parsing` — the
//!   PP-StructureV3 pipeline. `result.layoutParsingResults[i].prunedResult.parsing_res_list[]`
//!   gives `block_label`, `block_content` (tables as HTML) and `block_bbox` in reading order.
//!
//! - `vl` model, `parse` (and `ocr` derived from it): `POST {PADDLEOCR_VL_BASE_URL or
//!   PADDLEOCR_BASE_URL}/layout-parsing` on a PaddleOCR-VL pipeline server (PaddleOCR-VL-1.6 by
//!   default: layout detection + the 0.9B VLM). Same request and `layoutParsingResults` shape as
//!   PP-StructureV3, so the same normaliser applies.
//!
//! All take `{"file": <base64>, "fileType": 0 (PDF) | 1 (image)}` (URL inputs are downloaded first,
//! through `crate::fetch`) and answer
//! `{logId, errorCode, errorMsg, result: {..., dataInfo}}`; `dataInfo` gives each page's pixel
//! size in the same space as the boxes. `provider_options` are merged into the request body
//! (e.g. `useDocOrientationClassify`, `textRecScoreThresh`). No API key.
//!
//! By default the server only processes the first 10 pages of a PDF
//! (`Serving.extra.max_num_input_imgs` in the pipeline config); when fewer pages come back than
//! `dataInfo.numPages` reports, `paddleocr_pages_truncated` is set in the response metadata.

use crate::error::{Error, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
use crate::providers::local::{self, FileKind};
use crate::providers::unstructured::{html_table_to_markdown, html_to_text};
use crate::types::{
    BBox, Block, BlockType, DocumentRequest, Line, OutputFormat, Page, ParseResponse, TextPage, TextResponse, Usage,
    Word,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const NAME: &str = "paddleocr";
const ENV_BASE: &str = "PADDLEOCR_BASE_URL";
const ENV_PARSE_BASE: &str = "PADDLEOCR_PARSE_BASE_URL";
const ENV_VL_BASE: &str = "PADDLEOCR_VL_BASE_URL";
/// The registry model served by a PaddleOCR-VL pipeline server.
const VL_MODEL: &str = "vl";
const DEFAULT_BASE: &str = "http://localhost:8080";

#[derive(Debug, Default, Clone, Copy)]
pub struct PaddleOcr;

#[async_trait]
impl Provider for PaddleOcr {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn ocr(&self, request: &DocumentRequest, model: &str) -> Result<TextResponse> {
        if model == VL_MODEL {
            // PaddleOCR-VL has no OCR-only endpoint: derive the text from the layout parse.
            let parsed = self.parse(request, model).await?;
            return Ok(TextResponse::from_parse(&parsed));
        }
        let base = provider::resolve_base_url(request, ENV_BASE, DEFAULT_BASE);
        let result = call(request, &format!("{base}/ocr")).await?;
        let ranges = request.pages.as_deref().map(crate::util::parse_page_ranges).transpose()?;
        let mut resp = normalize_ocr(&result, model, ranges.as_deref())?;
        if request.include_raw {
            resp.raw = Some(result);
        }
        Ok(resp)
    }

    async fn parse(&self, request: &DocumentRequest, model: &str) -> Result<ParseResponse> {
        // An explicit `base_url` on the request wins; otherwise the parse-specific env var, then
        // the shared one, since PP-StructureV3 is usually served separately from the OCR pipeline.
        let parse_env = if model == VL_MODEL { ENV_VL_BASE } else { ENV_PARSE_BASE };
        let base = match &request.base_url {
            Some(_) => provider::resolve_base_url(request, parse_env, DEFAULT_BASE),
            None => std::env::var(parse_env)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|v| v.trim_end_matches('/').to_string())
                .unwrap_or_else(|| provider::resolve_base_url(request, ENV_BASE, DEFAULT_BASE)),
        };
        let result = call(request, &format!("{base}/layout-parsing")).await?;
        let ranges = request.pages.as_deref().map(crate::util::parse_page_ranges).transpose()?;
        let mut resp = normalize_parse(&result, request.output, model, ranges.as_deref())?;
        if request.include_raw {
            resp.raw = Some(result);
        }
        Ok(resp)
    }
}

/// POST the document and return the whole response body (`result` is checked by the caller).
async fn call(request: &DocumentRequest, url: &str) -> Result<Value> {
    let deadline = Deadline::new(request.timeout_secs);
    let retry = Retry::new(request.max_retries);
    let body = request_body(request, &deadline).await?;
    let client = http::client();
    http::with_retry(NAME, retry, &deadline, || {
        let rb = client.post(url).timeout(deadline.request_timeout()).json(&body);
        async move {
            let resp = rb.send().await.map_err(|e| connect_hint(e, url))?;
            let status = resp.status().as_u16();
            let text = resp.text().await?;
            check_envelope(status, &text)
        }
    })
    .await
}

fn connect_hint(e: reqwest::Error, url: &str) -> Error {
    if e.is_connect() {
        Error::network(format!(
            "cannot reach PaddleOCR serving at {url} ({e}); start it (`paddlex --serve --pipeline OCR` / \
             `--pipeline PP-StructureV3` / `--pipeline PaddleOCR-VL`) or set {ENV_BASE} / {ENV_PARSE_BASE} / \
             {ENV_VL_BASE}"
        ))
        .with_provider(NAME)
    } else {
        e.into()
    }
}

/// Map the serving envelope to a result or an error. Failures carry `errorCode` (= HTTP status)
/// and `errorMsg`, which is passed through verbatim.
fn check_envelope(status: u16, body: &str) -> Result<Value> {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let code = v.as_ref().and_then(|v| v.get("errorCode")).and_then(Value::as_i64).unwrap_or(0);
    let msg = v.as_ref().and_then(|v| v.get("errorMsg")).and_then(Value::as_str).unwrap_or_default().to_string();
    if !(200..300).contains(&status) || code != 0 {
        let http_status = if (200..300).contains(&status) { u16::try_from(code).unwrap_or(500) } else { status };
        let message = if msg.is_empty() { body.to_string() } else { msg };
        return Err(Error::from_http(NAME, http_status, &json!({ "message": message }).to_string()));
    }
    let v = v.ok_or_else(|| {
        Error::provider(format!("unexpected response (not JSON): {}", http::snippet(body))).with_provider(NAME)
    })?;
    if v.get("result").map(Value::is_object) != Some(true) {
        return Err(
            Error::provider(format!("response has no result object: {}", http::snippet(body))).with_provider(NAME)
        );
    }
    Ok(v)
}

async fn request_body(request: &DocumentRequest, deadline: &Deadline) -> Result<Value> {
    // URL inputs are downloaded here (address-filtered, see `crate::fetch`) rather than handed to
    // the serving endpoint: that server sits on the operator's network, and letting it fetch a
    // caller's URL would be a server-side request from inside it.
    let data = local::load_or_download(NAME, request, deadline).await?;
    let file_type = match local::sniff(&data, &request.input.filename()) {
        FileKind::Pdf => 0,
        FileKind::Image => 1,
        FileKind::Other => {
            return Err(Error::input(format!(
                "paddleocr reads images and PDFs only; '{}' is neither",
                request.input.filename()
            )))
        }
    };
    let mut body = json!({"file": local::base64_encode(&data), "fileType": file_type, "visualize": false});
    if let Some(extra) = &request.provider_options {
        if !extra.is_object() {
            return Err(Error::input("paddleocr: provider_options must be an object (request body fields)"));
        }
        crate::util::deep_merge(&mut body, extra);
    }
    Ok(body)
}

// ---- response → unified types ------------------------------------------------------------------

/// Pixel size of page `i` (0-based) from `dataInfo`, falling back to the page's own result.
fn page_dims(data_info: Option<&Value>, i: usize, pruned: &Value) -> Option<(f64, f64)> {
    let wh = |v: &Value| Some((v.get("width")?.as_f64()?, v.get("height")?.as_f64()?));
    data_info
        .and_then(|d| d.get("pages").and_then(Value::as_array).and_then(|p| p.get(i)).and_then(wh))
        .or_else(|| data_info.filter(|d| d.get("type").and_then(Value::as_str) == Some("image")).and_then(wh))
        .or_else(|| wh(pruned))
}

/// An axis-aligned box from `[x0, y0, x1, y1]` or a polygon `[[x, y], ...]`.
fn px_box(v: &Value, dims: Option<(f64, f64)>) -> Option<BBox> {
    let (w, h) = dims?;
    let arr = v.as_array()?;
    let (x0, y0, x1, y1) = if arr.first()?.is_array() {
        let pts: Vec<(f64, f64)> =
            arr.iter().filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?))).collect();
        if pts.is_empty() {
            return None;
        }
        pts.iter().fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |(a, b, c, d), &(x, y)| {
            (a.min(x), b.min(y), c.max(x), d.max(y))
        })
    } else {
        let n: Vec<f64> = arr.iter().filter_map(Value::as_f64).collect();
        if n.len() < 4 {
            return None;
        }
        (n[0].min(n[2]), n[1].min(n[3]), n[0].max(n[2]), n[1].max(n[3]))
    };
    BBox::from_xywh(x0, y0, x1 - x0, y1 - y0, w, h)
}

fn truncation_note(result: &Value, returned: usize) -> Option<Value> {
    let total = result.pointer("/result/dataInfo/numPages").and_then(Value::as_u64)? as usize;
    (returned < total).then(|| json!({"returned": returned, "document_pages": total}))
}

pub(crate) fn normalize_ocr(
    result: &Value,
    model: &str,
    ranges: Option<&[(u32, Option<u32>)]>,
) -> Result<TextResponse> {
    let pages_v = result
        .pointer("/result/ocrResults")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::provider("response has no result.ocrResults").with_provider(NAME))?;
    let data_info = result.pointer("/result/dataInfo");
    let mut pages = Vec::new();
    for (i, page) in pages_v.iter().enumerate() {
        let page_number = i as u32 + 1;
        if !local::page_selected(ranges, page_number) {
            continue;
        }
        let pruned = page.get("prunedResult").unwrap_or(&Value::Null);
        let dims = page_dims(data_info, i, pruned);
        let texts = pruned.get("rec_texts").and_then(Value::as_array).cloned().unwrap_or_default();
        let scores = pruned.get("rec_scores").and_then(Value::as_array).cloned().unwrap_or_default();
        let boxes = pruned
            .get("rec_boxes")
            .or_else(|| pruned.get("rec_polys"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut lines = Vec::new();
        for (j, t) in texts.iter().enumerate() {
            let Some(text) = t.as_str().map(str::trim).filter(|s| !s.is_empty()) else { continue };
            lines.push(Line {
                text: text.to_string(),
                bbox: boxes.get(j).and_then(|b| px_box(b, dims)),
                confidence: scores.get(j).and_then(Value::as_f64),
            });
        }
        // PaddleOCR recognises text lines; words are split out without their own geometry.
        let words: Vec<Word> = lines
            .iter()
            .flat_map(|l| {
                l.text.split_whitespace().map(|w| Word { text: w.to_string(), bbox: None, confidence: l.confidence })
            })
            .collect();
        pages.push(TextPage {
            page_number,
            width: dims.map(|d| d.0),
            height: dims.map(|d| d.1),
            text: lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"),
            lines,
            words,
        });
    }
    let usage = Usage { pages: pages.len() as u32, credits: None, provider_cost_usd: None };
    let mut resp = TextResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage);
    resp.metadata = envelope_metadata(result, pages_v.len());
    Ok(resp)
}

pub(crate) fn normalize_parse(
    result: &Value,
    fmt: OutputFormat,
    model: &str,
    ranges: Option<&[(u32, Option<u32>)]>,
) -> Result<ParseResponse> {
    let pages_v = result
        .pointer("/result/layoutParsingResults")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::provider("response has no result.layoutParsingResults").with_provider(NAME))?;
    let data_info = result.pointer("/result/dataInfo");
    let mut blocks = Vec::new();
    let mut dims_map = BTreeMap::new();
    for (i, page) in pages_v.iter().enumerate() {
        let page_number = i as u32 + 1;
        if !local::page_selected(ranges, page_number) {
            continue;
        }
        let pruned = page.get("prunedResult").unwrap_or(&Value::Null);
        let dims = page_dims(data_info, i, pruned);
        if let Some(d) = dims {
            dims_map.insert(page_number, d);
        }
        for b in pruned.get("parsing_res_list").and_then(Value::as_array).into_iter().flatten() {
            let label = b.get("block_label").and_then(Value::as_str).unwrap_or_default();
            let raw = b.get("block_content").and_then(Value::as_str).unwrap_or_default().trim();
            let bbox = b.get("block_bbox").and_then(|v| px_box(v, dims));
            let (block_type, markdown, text) = map_block(label, raw);
            if markdown.is_empty() && block_type != BlockType::Figure {
                continue;
            }
            let content = if fmt == OutputFormat::Text { text.clone() } else { markdown };
            blocks.push(Block { block_type, content, text: Some(text), bbox, confidence: None, page_number });
        }
    }
    let mut pages = crate::types::pages_from_blocks(blocks, &dims_map);
    for (i, _) in pages_v.iter().enumerate() {
        let n = i as u32 + 1;
        if local::page_selected(ranges, n) && !pages.iter().any(|p| p.page_number == n) {
            let d = dims_map.get(&n);
            pages.push(Page {
                page_number: n,
                width: d.map(|d| d.0),
                height: d.map(|d| d.1),
                markdown: String::new(),
                text: String::new(),
                blocks: vec![],
            });
        }
    }
    pages.sort_by_key(|p| p.page_number);
    let usage = Usage { pages: pages.len() as u32, credits: None, provider_cost_usd: None };
    let mut resp = ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage);
    resp.metadata = envelope_metadata(result, pages_v.len());
    Ok(resp)
}

fn envelope_metadata(result: &Value, returned: usize) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    if let Some(id) = result.get("logId").and_then(Value::as_str) {
        m.insert("paddleocr_log_id".into(), json!(id));
    }
    if let Some(note) = truncation_note(result, returned) {
        m.insert("paddleocr_pages_truncated".into(), note);
    }
    m
}

/// PP-StructureV3 layout labels → block type, markdown, plain text.
fn map_block(label: &str, content: &str) -> (BlockType, String, String) {
    let plain = || content.to_string();
    match label {
        "doc_title" => (BlockType::Title, format!("# {content}"), plain()),
        "paragraph_title" => (BlockType::SectionHeader, format!("## {content}"), plain()),
        "table" => {
            let md = html_table_to_markdown(content).unwrap_or_else(|| content.to_string());
            let text = if content.contains('<') { table_html_text(content) } else { plain() };
            (BlockType::Table, md, text)
        }
        "image" | "chart" | "seal" | "header_image" | "footer_image" => (BlockType::Figure, plain(), plain()),
        "figure_title" | "table_title" | "chart_title" | "figure_table_chart_title" => {
            (BlockType::Caption, plain(), plain())
        }
        "formula" | "display_formula" | "inline_formula" => {
            let md = if content.starts_with('$') { content.to_string() } else { format!("$$\n{content}\n$$") };
            (BlockType::Formula, md, plain())
        }
        "header" => (BlockType::Header, plain(), plain()),
        "footer" | "number" => (BlockType::Footer, plain(), plain()),
        "footnote" | "vision_footnote" => (BlockType::Footnote, plain(), plain()),
        "algorithm" => (BlockType::Other, format!("```\n{content}\n```"), plain()),
        "formula_number" => (BlockType::Other, plain(), plain()),
        _ => (BlockType::Text, plain(), plain()), // text, content, abstract, reference, aside_text, ...
    }
}

/// Table HTML → one line per row, cells separated by spaces.
pub(crate) fn table_html_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut rows = Vec::new();
    let mut pos = 0;
    while let Some(off) = lower[pos..].find("<tr") {
        let start = pos + off;
        let end = lower[start..].find("</tr>").map(|e| start + e).unwrap_or(lower.len());
        let (row, row_l) = (&html[start..end], &lower[start..end]);
        // Cell starts: every `<td` / `<th` (not `<thead`), each running to the next cell start.
        let starts: Vec<usize> = row_l
            .match_indices("<t")
            .map(|(i, _)| i)
            .filter(|&i| matches!(row_l.as_bytes().get(i + 3), Some(b'>' | b' ' | b'\t' | b'\n' | b'/')))
            .filter(|&i| matches!(row_l.as_bytes().get(i + 2), Some(b'd' | b'h')))
            .collect();
        let cells: Vec<String> = starts
            .iter()
            .enumerate()
            .map(|(k, &a)| html_to_text(&row[a..starts.get(k + 1).copied().unwrap_or(row.len())]))
            .filter(|c| !c.is_empty())
            .collect();
        rows.push(cells.join(" "));
        pos = end.max(start + 3);
    }
    if rows.is_empty() {
        html_to_text(html)
    } else {
        rows.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "ocr" => include_str!("../../tests/fixtures/paddleocr_ocr.json"),
            "layout" => include_str!("../../tests/fixtures/paddleocr_layout_parsing.json"),
            "vl" => include_str!("../../tests/fixtures/paddleocr_vl_layout_parsing.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn normalizes_ocr_results() {
        let v = check_envelope(200, &fixture("ocr").to_string()).unwrap();
        let resp = normalize_ocr(&v, "default", None).unwrap();
        assert_eq!(resp.pages.len(), 2);
        let p = &resp.pages[0];
        assert_eq!((p.width, p.height), (Some(1224.0), Some(1584.0)));
        assert_eq!(p.lines[0].text, "Quarterly Report");
        assert_eq!(p.lines[0].confidence, Some(0.998));
        let bb = p.lines[0].bbox.unwrap();
        assert!((bb.x0 - 100.0 / 1224.0).abs() < 1e-9 && (bb.y1 - 130.0 / 1584.0).abs() < 1e-9, "{bb:?}");
        assert_eq!(p.words.len(), 2 + 7);
        // rec_polys fallback on page 2, and empty detections dropped.
        let p2 = &resp.pages[1];
        assert_eq!(p2.lines.len(), 1);
        assert!(p2.lines[0].bbox.is_some());
        assert_eq!(resp.text, "Quarterly Report\nRevenue grew 12% over the prior quarter.\n\nPage two text");
        assert_eq!(resp.metadata["paddleocr_log_id"], "3b1b2c7e-2f0e-4d7c-9a51-0f5ad0b8b0a1");
        assert!(!resp.metadata.contains_key("paddleocr_pages_truncated"));

        let only2 = normalize_ocr(&v, "default", Some(&[(2, Some(2))])).unwrap();
        assert_eq!(only2.pages.len(), 1);
        assert_eq!(only2.pages[0].page_number, 2);
    }

    #[test]
    fn normalizes_paddleocr_vl_results() {
        let v = check_envelope(200, &fixture("vl").to_string()).unwrap();
        let resp = normalize_parse(&v, OutputFormat::Markdown, VL_MODEL, None).unwrap();
        assert_eq!(resp.model, "paddleocr/vl");
        assert_eq!(resp.pages.len(), 2);
        let p = &resp.pages[0];
        assert_eq!((p.width, p.height), (Some(1224.0), Some(1584.0)));
        let types: Vec<BlockType> = p.blocks.iter().map(|b| b.block_type).collect();
        assert_eq!(
            types,
            [
                BlockType::Title,
                BlockType::Text,
                BlockType::Table,
                BlockType::Formula,
                BlockType::Figure,
                BlockType::Footnote,
                BlockType::Footer
            ]
        );
        assert_eq!(p.blocks[0].content, "# Cedar Ridge Supply");
        assert!(p.blocks[2].content.contains("| Fence post | 12 | $8.50 |"), "{}", p.blocks[2].content);
        assert_eq!(p.blocks[2].text.as_deref(), Some("Item Qty Price\nFence post 12 $8.50"));
        assert!(p.blocks[3].content.starts_with("$$"));
        let bb = p.blocks[0].bbox.unwrap();
        assert!((bb.x0 - 122.0 / 1224.0).abs() < 1e-9 && (bb.y1 - 158.0 / 1584.0).abs() < 1e-9, "{bb:?}");
        assert_eq!(resp.pages[1].blocks[0].block_type, BlockType::SectionHeader);
        assert!(!resp.metadata.contains_key("paddleocr_pages_truncated"));
    }

    #[tokio::test]
    async fn vl_ocr_is_derived_from_the_layout_parsing_endpoint() {
        let body = fixture("vl").to_string();
        let (base, captured) = crate::testutil::serve(vec![(200, body)]).await;
        let req = DocumentRequest::from_bytes(&b"%PDF-1.7 fake"[..], "invoice.pdf").base_url(&base);
        let resp = PaddleOcr.ocr(&req, VL_MODEL).await.unwrap();
        assert_eq!(resp.pages.len(), 2);
        assert!(resp.text.contains("Payment due within 30 days."));
        let seen = captured.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].route(), "POST /layout-parsing", "no /ocr endpoint on a PaddleOCR-VL server");
        let sent: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(sent["fileType"], 0);
        assert_eq!(sent["visualize"], false);
    }

    #[test]
    fn normalizes_layout_parsing_results() {
        let v = fixture("layout");
        let resp = normalize_parse(&v, OutputFormat::Markdown, "default", None).unwrap();
        assert_eq!(resp.pages.len(), 1);
        let blocks = &resp.pages[0].blocks;
        let types: Vec<BlockType> = blocks.iter().map(|b| b.block_type).collect();
        assert_eq!(
            types,
            [
                BlockType::Header,
                BlockType::Title,
                BlockType::SectionHeader,
                BlockType::Text,
                BlockType::Caption,
                BlockType::Table,
                BlockType::Formula,
                BlockType::Figure,
                BlockType::Footer
            ]
        );
        assert_eq!(blocks[1].content, "# Annual Summary");
        let table = &blocks[5];
        assert!(table.content.starts_with("| Region | Sales |"), "{}", table.content);
        assert_eq!(table.text.as_deref(), Some("Region Sales\nNorth 120\nSouth 95"));
        assert!(blocks[6].content.starts_with("$$\n"));
        assert!(blocks.iter().all(|b| b.bbox.is_some()));
        let bb = blocks[1].bbox.unwrap();
        assert!((bb.x0 - 0.1).abs() < 1e-9 && (bb.y0 - 0.05).abs() < 1e-9, "{bb:?}");
        assert!(resp.markdown.contains("## 1. Overview"));
        assert_eq!(resp.metadata["paddleocr_pages_truncated"], json!({"returned": 1, "document_pages": 12}));

        let text = normalize_parse(&v, OutputFormat::Text, "default", None).unwrap();
        assert!(text.pages[0].markdown.starts_with("ACME Corp"));
    }

    #[test]
    fn envelope_errors_keep_the_server_message() {
        let e = check_envelope(422, r#"{"logId":"x","errorCode":422,"errorMsg":"Invalid file type"}"#).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::BadRequest);
        assert!(e.message.contains("Invalid file type"), "{e}");
        let e = check_envelope(200, r#"{"logId":"x","errorCode":500,"errorMsg":"Internal inference failure"}"#)
            .unwrap_err();
        assert_eq!(e.status_code, Some(500));
        assert!(e.message.contains("Internal inference failure"));
        assert!(check_envelope(200, "<html>").is_err());
        assert!(check_envelope(200, r#"{"errorCode":0}"#).is_err());
    }

    #[tokio::test]
    async fn request_body_sets_file_type_and_merges_options() {
        let dl = Deadline::new(5.0);
        let req =
            DocumentRequest::from_bytes(&b"%PDF-1.4"[..], "a.pdf").provider_options(json!({"useDocUnwarping": false}));
        let b = request_body(&req, &dl).await.unwrap();
        assert_eq!(b["fileType"], 0);
        assert_eq!(b["file"], "JVBERi0xLjQ=");
        assert_eq!(b["visualize"], false);
        assert_eq!(b["useDocUnwarping"], false);
        // URLs are fetched in-process through the filtered path, never forwarded to the server.
        let e = request_body(&DocumentRequest::from_url("http://127.0.0.1/scan.png"), &dl).await.unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Input, "{e}");
        assert!(request_body(&DocumentRequest::from_bytes(&b"PK\x03\x04"[..], "a.docx"), &dl).await.is_err());
    }

    /// Needs PaddleOCR serving (`PADDLEOCR_BASE_URL`, default http://localhost:8080). Run with:
    /// `cargo test -p puffinparse-core paddleocr -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a running PaddleOCR / PaddleX serving endpoint"]
    async fn live_ocr() {
        let sample = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/plain_001.png");
        let req = DocumentRequest::from_path(sample).model("paddleocr/default").timeout_secs(300.0);
        let resp = PaddleOcr.ocr(&req, "default").await.expect("paddleocr serving answers");
        assert_eq!(resp.pages.len(), 1);
        assert!(!resp.pages[0].lines.is_empty());
    }
}
