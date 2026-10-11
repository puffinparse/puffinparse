//! Docling, through a self-hosted [docling-serve](https://github.com/docling-project/docling-serve).
//!
//! Flow (docling-serve v1 API): `POST /v1/convert/source/async` with a JSON body
//! `{options, sources: [{kind: "file", base64_string, filename} | {kind: "http", url}]}` →
//! `{task_id, task_status}` → poll `GET /v1/status/poll/{task_id}` until `success` / `failure` →
//! `GET /v1/result/{task_id}` → `{document: {json_content, ...}, status, errors, confidence}`.
//! The async flow is used because the synchronous endpoint is capped by the server's
//! `DOCLING_SERVE_MAX_SYNC_WAIT` (120 s by default) and long documents exceed it.
//!
//! `json_content` is a DoclingDocument: `body` (and `furniture`, for page headers/footers) hold
//! the reading order as `$ref`s into `texts`, `tables`, `pictures` and `groups`. Every item's
//! `prov[]` gives the page and a box whose `coord_origin` is usually `BOTTOMLEFT` (PDF points,
//! y up) — converted here to PuffinParse's top-left normalised boxes.
//!
//! Configuration: `DOCLING_BASE_URL` (default `http://localhost:5001`) or `base_url`;
//! `DOCLING_API_KEY` / `api_key` only when the server enforces `DOCLING_SERVE_API_KEY`
//! (sent as `X-Api-Key`). `provider_options` are merged into `options` verbatim.

use crate::error::{Error, Result};
use crate::http::{self, Deadline, Retry};
use crate::provider::{self, Provider};
use crate::providers::local;
use crate::types::{BBox, Block, BlockType, DocumentRequest, OutputFormat, Page, ParseResponse, Usage};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub const NAME: &str = "docling";
const ENV_KEY: &str = "DOCLING_API_KEY";
const ENV_BASE: &str = "DOCLING_BASE_URL";
const DEFAULT_BASE: &str = "http://localhost:5001";

#[derive(Debug, Default, Clone, Copy)]
pub struct Docling;

#[async_trait]
impl Provider for Docling {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn parse(&self, request: &DocumentRequest, model: &str) -> Result<ParseResponse> {
        let base = provider::resolve_base_url(request, ENV_BASE, DEFAULT_BASE);
        let api_key = request.api_key.clone().or_else(|| std::env::var(ENV_KEY).ok()).filter(|k| !k.trim().is_empty());
        let deadline = Deadline::new(request.timeout_secs);
        let retry = Retry::new(request.max_retries);
        let client = http::client();
        let ranges = request.pages.as_deref().map(crate::util::parse_page_ranges).transpose()?;
        let body = request_body(request, ranges.as_deref(), &deadline).await?;

        let with_key = |rb: reqwest::RequestBuilder| match &api_key {
            Some(k) => rb.header("X-Api-Key", k),
            None => rb,
        };

        // 1. Submit.
        let task: Value = http::with_retry(NAME, retry, &deadline, || {
            let rb = with_key(client.post(format!("{base}/v1/convert/source/async")))
                .timeout(deadline.request_timeout())
                .json(&body);
            async move { http::read_json(NAME, rb.send().await.map_err(connect_hint)?).await }
        })
        .await?;
        let task_id = task.get("task_id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| {
            Error::provider(format!("no task_id in {}", http::snippet(&task.to_string()))).with_provider(NAME)
        })?;
        tracing::debug!(%task_id, "docling: task submitted");

        // 2. Poll.
        let status: Value =
            http::poll_until(NAME, &deadline, Duration::from_millis(500), Duration::from_secs(5), || {
                let rb = with_key(client.get(format!("{base}/v1/status/poll/{task_id}")))
                    .timeout(deadline.request_timeout());
                async move {
                    let s: Value = http::read_json(NAME, rb.send().await?).await?;
                    let st = s.get("task_status").and_then(Value::as_str).unwrap_or_default();
                    Ok(matches!(st, "success" | "failure" | "partial_success" | "skipped").then_some(s))
                }
            })
            .await
            .map_err(|e| e.with_job_id(task_id.clone()))?;
        if status.get("task_status").and_then(Value::as_str) == Some("failure") {
            let msg = task_failure_message(&status);
            return Err(Error::provider(format!("conversion failed: {msg}")).with_provider(NAME).with_job_id(task_id));
        }

        // 3. Result.
        let rb = with_key(client.get(format!("{base}/v1/result/{task_id}"))).timeout(deadline.request_timeout());
        let result: Value = http::with_retry(NAME, retry, &deadline, || {
            let rb = rb.try_clone().expect("GET without a streaming body clones");
            async move { http::read_json(NAME, rb.send().await?).await }
        })
        .await
        .map_err(|e| e.with_job_id(task_id.clone()))?;

        let mut resp =
            normalize(&result, request.output, model, ranges.as_deref()).map_err(|e| e.with_job_id(task_id.clone()))?;
        resp.provider_job_id = Some(task_id);
        if request.include_raw {
            resp.raw = Some(result);
        }
        Ok(resp)
    }
}

/// Point a refused connection at the likely cause: no docling-serve running at the base URL.
fn connect_hint(e: reqwest::Error) -> Error {
    if e.is_connect() {
        Error::network(format!(
            "cannot reach docling-serve ({e}); start one (`docker run -p 5001:5001 quay.io/docling-project/docling-serve` \
             or `pip install docling-serve && docling-serve run`) or set {ENV_BASE}"
        ))
        .with_provider(NAME)
    } else {
        e.into()
    }
}

fn task_failure_message(status: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(m) = status.get("error_message").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        parts.push(m.to_string());
    }
    if let Some(f) = status.get("failure").filter(|f| !f.is_null()) {
        parts.push(f.to_string());
    }
    if parts.is_empty() {
        "task_status=failure with no message".into()
    } else {
        parts.join("; ")
    }
}

async fn request_body(
    request: &DocumentRequest,
    ranges: Option<&[(u32, Option<u32>)]>,
    deadline: &Deadline,
) -> Result<Value> {
    let mut options = Map::new();
    options.insert("to_formats".into(), json!(["json"]));
    options.insert("image_export_mode".into(), json!("placeholder"));
    // Picture crops are not used by the mapping; skipping them keeps the result small.
    options.insert("include_images".into(), json!(false));
    if let Some(r) = ranges {
        // docling takes one inclusive span; pages outside the selection are dropped afterwards.
        let first = r.iter().map(|(s, _)| *s).min().unwrap_or(1);
        let last = r.iter().map(|(_, e)| *e).try_fold(0u32, |acc, e| e.map(|e| acc.max(e)));
        options.insert("page_range".into(), json!([first, last.map(i64::from).unwrap_or(i64::MAX)]));
    }
    if let Some(lang) = &request.language {
        options.insert("ocr_lang".into(), json!([lang]));
    }
    let mut options = Value::Object(options);
    if let Some(extra) = &request.provider_options {
        if !extra.is_object() {
            return Err(Error::input("docling: provider_options must be an object (docling-serve `options`)"));
        }
        crate::util::deep_merge(&mut options, extra);
    }
    // A URL is downloaded here (address-filtered, see `crate::fetch`) and sent as a file, never as
    // an `http` source: docling-serve sits on the operator's network, and letting it fetch a
    // caller's URL would be a server-side request from inside it.
    let data = local::load_or_download(NAME, request, deadline).await?;
    let source =
        json!({"kind": "file", "base64_string": local::base64_encode(&data), "filename": request.input.filename()});
    Ok(json!({"options": options, "sources": [source]}))
}

// ---- DoclingDocument → unified types -------------------------------------------------------------

/// Where a block sits relative to the body within its page: headers first, footers last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Band {
    Header,
    Body,
    Footer,
}

struct Walker<'a> {
    doc: &'a Value,
    dims: BTreeMap<u32, (f64, f64)>,
    fmt: OutputFormat,
    out: Vec<(Band, Block)>,
    seen: BTreeSet<String>,
}

pub(crate) fn normalize(
    result: &Value,
    fmt: OutputFormat,
    model: &str,
    ranges: Option<&[(u32, Option<u32>)]>,
) -> Result<ParseResponse> {
    let status = result.get("status").and_then(Value::as_str).unwrap_or("success");
    let errors: Vec<String> = result
        .get("errors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|e| {
                    e.get("error_message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| e.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    if matches!(status, "failure" | "skipped") {
        let msg = if errors.is_empty() { format!("status={status}") } else { errors.join("; ") };
        return Err(Error::provider(format!("conversion {status}: {msg}")).with_provider(NAME));
    }
    let doc = result
        .pointer("/document/json_content")
        .filter(|v| v.is_object())
        .ok_or_else(|| Error::provider("result has no document.json_content").with_provider(NAME))?;

    let mut dims = BTreeMap::new();
    if let Some(pages) = doc.get("pages").and_then(Value::as_object) {
        for (k, p) in pages {
            let n = p.get("page_no").and_then(Value::as_u64).map(|n| n as u32).or_else(|| k.parse().ok());
            let w = p.pointer("/size/width").and_then(Value::as_f64);
            let h = p.pointer("/size/height").and_then(Value::as_f64);
            if let (Some(n), Some(w), Some(h)) = (n, w, h) {
                dims.insert(n, (w, h));
            }
        }
    }

    let mut walker = Walker { doc, dims: dims.clone(), fmt, out: Vec::new(), seen: BTreeSet::new() };
    if let Some(furniture) = doc.get("furniture") {
        walker.walk_children(furniture, Band::Header);
    }
    if let Some(body) = doc.get("body") {
        walker.walk_children(body, Band::Body);
    }
    let mut placed = walker.out;
    // Stable: body keeps docling's reading order; furniture goes to the top or bottom of its page.
    placed.sort_by_key(|(band, b)| (b.page_number, *band));
    let blocks: Vec<Block> =
        placed.into_iter().map(|(_, b)| b).filter(|b| local::page_selected(ranges, b.page_number)).collect();

    let mut pages: Vec<Page> = crate::types::pages_from_blocks(blocks, &dims);
    for (&n, &(w, h)) in &dims {
        if local::page_selected(ranges, n) && !pages.iter().any(|p| p.page_number == n) {
            pages.push(Page {
                page_number: n,
                width: Some(w),
                height: Some(h),
                markdown: String::new(),
                text: String::new(),
                blocks: vec![],
            });
        }
    }
    pages.sort_by_key(|p| p.page_number);
    if let OutputFormat::Text = fmt {
        for p in &mut pages {
            p.markdown = p.text.clone();
        }
    }

    let usage = Usage { pages: pages.len() as u32, credits: None, provider_cost_usd: None };
    let mut resp = ParseResponse::from_pages(NAME, &format!("{NAME}/{model}"), pages, usage);
    resp.metadata.insert("docling_status".into(), json!(status));
    if let Some(t) = result.get("processing_time").filter(|v| v.is_number()) {
        resp.metadata.insert("docling_processing_time_s".into(), t.clone());
    }
    if let Some(c) = result.get("confidence").filter(|v| v.is_object()) {
        resp.metadata.insert("docling_confidence".into(), c.clone());
    }
    if !errors.is_empty() {
        resp.metadata.insert("docling_errors".into(), json!(errors));
    }
    Ok(resp)
}

impl<'a> Walker<'a> {
    fn resolve(&self, reference: &str) -> Option<&'a Value> {
        // "#/texts/3" → doc["texts"][3]
        self.doc.pointer(reference.strip_prefix('#')?)
    }

    fn walk_children(&mut self, node: &'a Value, band: Band) {
        for child in node.get("children").and_then(Value::as_array).into_iter().flatten() {
            if let Some(r) = child.get("$ref").and_then(Value::as_str) {
                self.walk_ref(r, band);
            }
        }
    }

    fn walk_ref(&mut self, reference: &str, band: Band) {
        if !self.seen.insert(reference.to_string()) {
            return;
        }
        let Some(item) = self.resolve(reference) else { return };
        let label = item.get("label").and_then(Value::as_str).unwrap_or_default();
        if reference.starts_with("#/groups/") {
            if label == "inline" {
                self.inline_group(item, band);
            } else {
                self.walk_children(item, band);
            }
        } else if reference.starts_with("#/tables/") {
            self.table(item, band);
        } else if reference.starts_with("#/pictures/") {
            // Picture children are text found inside the image; docling's own markdown omits them.
            self.emit_item(item, BlockType::Figure, String::new(), String::new(), band);
            self.captions(item, band);
        } else if reference.starts_with("#/texts/") {
            self.text(item, label, band);
            self.walk_children(item, band);
        }
        // key_value_items / form_items carry no reading-order text of their own.
    }

    fn text(&mut self, item: &'a Value, label: &str, band: Band) {
        let text = item.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if text.is_empty() {
            return;
        }
        let band = match label {
            "page_header" => Band::Header,
            "page_footer" => Band::Footer,
            _ => band,
        };
        let (block_type, markdown) = match label {
            "title" => (BlockType::Title, format!("# {text}")),
            "section_header" => {
                let level = item.get("level").and_then(Value::as_u64).unwrap_or(1).clamp(1, 5) as usize;
                (BlockType::SectionHeader, format!("{} {text}", "#".repeat(level + 1)))
            }
            "list_item" => {
                let enumerated = item.get("enumerated").and_then(Value::as_bool).unwrap_or(false);
                let marker = item.get("marker").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty());
                let marker = match marker {
                    Some(m) if enumerated => m.to_string(),
                    _ if enumerated => "1.".to_string(),
                    _ => "-".to_string(),
                };
                (BlockType::List, format!("{marker} {text}"))
            }
            "caption" => (BlockType::Caption, text.clone()),
            "footnote" => (BlockType::Footnote, text.clone()),
            "page_header" => (BlockType::Header, text.clone()),
            "page_footer" => (BlockType::Footer, text.clone()),
            "formula" => (BlockType::Formula, format!("$$\n{text}\n$$")),
            "code" => (BlockType::Other, format!("```\n{text}\n```")),
            "checkbox_selected" => (BlockType::Text, format!("[x] {text}")),
            "checkbox_unselected" => (BlockType::Text, format!("[ ] {text}")),
            _ => (BlockType::Text, text.clone()),
        };
        self.emit_item(item, block_type, markdown, text, band);
    }

    /// `inline` groups are one paragraph split into formatted runs: emit them as one text block.
    fn inline_group(&mut self, group: &'a Value, band: Band) {
        let mut parts = Vec::new();
        let mut first: Option<&'a Value> = None;
        for child in group.get("children").and_then(Value::as_array).into_iter().flatten() {
            let Some(r) = child.get("$ref").and_then(Value::as_str) else { continue };
            self.seen.insert(r.to_string());
            if let Some(t) = self.resolve(r) {
                if let Some(s) = t.get("text").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    parts.push(s.trim().to_string());
                    first = first.or(Some(t));
                }
            }
        }
        if let Some(first) = first {
            let text = parts.join(" ");
            self.emit_item(first, BlockType::Text, text.clone(), text, band);
        }
    }

    fn table(&mut self, item: &'a Value, band: Band) {
        let rows = table_rows(item.get("data").unwrap_or(&Value::Null));
        let markdown = rows_to_markdown(&rows);
        let text = rows.iter().map(|r| r.join(" ")).collect::<Vec<_>>().join("\n");
        self.emit_item(item, BlockType::Table, markdown, text, band);
        self.captions(item, band);
    }

    fn captions(&mut self, item: &'a Value, band: Band) {
        for c in item.get("captions").and_then(Value::as_array).into_iter().flatten() {
            if let Some(r) = c.get("$ref").and_then(Value::as_str) {
                self.walk_ref(r, band);
            }
        }
    }

    /// One block per provenance entry: an item that spans pages is split on its `charspan`s.
    fn emit_item(&mut self, item: &Value, block_type: BlockType, markdown: String, text: String, band: Band) {
        let provs: Vec<&Value> =
            item.get("prov").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        let fmt = self.fmt;
        let content = |md: &str, txt: &str| if fmt == OutputFormat::Text { txt.to_string() } else { md.to_string() };
        if provs.len() <= 1 {
            let page = provs.first().and_then(|p| p.get("page_no")).and_then(Value::as_u64).unwrap_or(1) as u32;
            let bbox = provs.first().and_then(|p| self.bbox(p, page));
            let c = content(&markdown, &text);
            self.out.push((
                band,
                Block { block_type, content: c, text: Some(text), bbox, confidence: None, page_number: page },
            ));
            return;
        }
        let chars: Vec<char> = text.chars().collect();
        for (i, prov) in provs.iter().enumerate() {
            let page = prov.get("page_no").and_then(Value::as_u64).unwrap_or(1) as u32;
            let span = prov
                .get("charspan")
                .and_then(Value::as_array)
                .and_then(|s| Some((s.first()?.as_u64()? as usize, s.get(1)?.as_u64()? as usize)));
            let piece = match span {
                Some((a, b)) if a < b && b <= chars.len() => chars[a..b].iter().collect::<String>().trim().to_string(),
                // No usable span: keep the whole item on its first page only.
                _ if i == 0 => text.clone(),
                _ => continue,
            };
            let md = if i == 0 && piece == text { markdown.clone() } else { piece.clone() };
            let bbox = self.bbox(prov, page);
            self.out.push((
                band,
                Block {
                    block_type,
                    content: content(&md, &piece),
                    text: Some(piece),
                    bbox,
                    confidence: None,
                    page_number: page,
                },
            ));
        }
    }

    fn bbox(&self, prov: &Value, page: u32) -> Option<BBox> {
        let &(w, h) = self.dims.get(&page)?;
        docling_bbox(prov.get("bbox")?, w, h)
    }
}

/// Convert a docling `BoundingBox` (`l, t, r, b` + `coord_origin`) to a normalised top-left box.
pub(crate) fn docling_bbox(b: &Value, page_w: f64, page_h: f64) -> Option<BBox> {
    if page_w <= 0.0 || page_h <= 0.0 {
        return None;
    }
    let get = |k: &str| b.get(k).and_then(Value::as_f64);
    let (l, t, r, bottom) = (get("l")?, get("t")?, get("r")?, get("b")?);
    let (top_y, bottom_y) = match b.get("coord_origin").and_then(Value::as_str) {
        // PDF convention: y grows upwards from the bottom edge, so `t` > `b`.
        Some("BOTTOMLEFT") | None => (page_h - t, page_h - bottom),
        _ => (t, bottom),
    };
    let (y0, y1) = if top_y <= bottom_y { (top_y, bottom_y) } else { (bottom_y, top_y) };
    let (x0, x1) = if l <= r { (l, r) } else { (r, l) };
    Some(BBox {
        x0: (x0 / page_w).clamp(0.0, 1.0),
        y0: (y0 / page_h).clamp(0.0, 1.0),
        x1: (x1 / page_w).clamp(0.0, 1.0),
        y1: (y1 / page_h).clamp(0.0, 1.0),
    })
}

/// Table cell texts as rows: from `data.grid` (spans already expanded), else from `table_cells`.
fn table_rows(data: &Value) -> Vec<Vec<String>> {
    let cell_text = |c: &Value| c.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if let Some(grid) = data.get("grid").and_then(Value::as_array).filter(|g| !g.is_empty()) {
        return grid
            .iter()
            .map(|row| row.as_array().map(|r| r.iter().map(cell_text).collect()).unwrap_or_default())
            .collect();
    }
    let n_rows = data.get("num_rows").and_then(Value::as_u64).unwrap_or(0) as usize;
    let n_cols = data.get("num_cols").and_then(Value::as_u64).unwrap_or(0) as usize;
    if n_rows == 0 || n_cols == 0 || n_rows * n_cols > 1_000_000 {
        return vec![];
    }
    let mut rows = vec![vec![String::new(); n_cols]; n_rows];
    for c in data.get("table_cells").and_then(Value::as_array).into_iter().flatten() {
        let idx = |k: &str| c.get(k).and_then(Value::as_u64).map(|v| v as usize);
        let (Some(r0), Some(c0)) = (idx("start_row_offset_idx"), idx("start_col_offset_idx")) else { continue };
        let r1 = idx("end_row_offset_idx").unwrap_or(r0 + 1).min(n_rows);
        let c1 = idx("end_col_offset_idx").unwrap_or(c0 + 1).min(n_cols);
        for row in rows.iter_mut().take(r1).skip(r0) {
            for cell in row.iter_mut().take(c1).skip(c0) {
                *cell = cell_text(c);
            }
        }
    }
    rows
}

fn rows_to_markdown(rows: &[Vec<String>]) -> String {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
    let line = |r: &Vec<String>| {
        let cells: Vec<String> = (0..width).map(|i| esc(r.get(i).map(String::as_str).unwrap_or(""))).collect();
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![line(&rows[0]), format!("|{}", "---|".repeat(width))];
    out.extend(rows[1..].iter().map(line));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "multipage" => include_str!("../../tests/fixtures/docling_multipage.json"),
            "headings" => include_str!("../../tests/fixtures/docling_headings.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn normalizes_real_multipage_pdf() {
        let resp = normalize(&fixture("multipage"), OutputFormat::Markdown, "default", None).unwrap();
        assert_eq!(resp.pages.len(), 2);
        assert_eq!(resp.usage.pages, 2);
        assert_eq!(resp.pages[0].width, Some(595.2));
        let p1 = &resp.pages[0];
        assert_eq!(p1.blocks[0].block_type, BlockType::SectionHeader);
        assert_eq!(p1.blocks[0].content, "## A Short History of the Harbor");
        // BOTTOMLEFT → top-left: the heading (t=803.92 of 841.92) sits near the top of the page.
        let bb = p1.blocks[0].bbox.unwrap();
        assert!(bb.y0 < 0.06 && bb.y1 < 0.1 && bb.y0 < bb.y1, "{bb:?}");
        assert!((bb.y0 - (841.92 - 803.92) / 841.92).abs() < 1e-9);
        assert!((bb.x0 - 35.666666666666664 / 595.2).abs() < 1e-9);
        let table = resp.pages[1].blocks.iter().find(|b| b.block_type == BlockType::Table).unwrap();
        assert!(
            table
                .content
                .starts_with("| Site | Samples | Mean Depth | Variance |\n|---|---|---|---|\n| Hazel Bend | 205 |"),
            "{}",
            table.content
        );
        assert!(table.bbox.is_some());
        assert!(resp.markdown.contains("## Site Measurement Log"));
        assert!(resp.text.contains("Hazel Bend 205 3.12 2.493"), "{}", resp.text);
        assert_eq!(resp.metadata["docling_status"], "success");
        assert!(resp.metadata["docling_confidence"]["mean_score"].is_number());
    }

    #[test]
    fn maps_lists_and_headings_from_image() {
        let resp = normalize(&fixture("headings"), OutputFormat::Markdown, "default", None).unwrap();
        assert_eq!(resp.pages.len(), 1);
        let blocks = &resp.pages[0].blocks;
        let list = blocks.iter().find(|b| b.block_type == BlockType::List).unwrap();
        assert_eq!(list.content, "- Publish the revised schedule on the notice board each Monday.");
        assert_eq!(blocks.iter().filter(|b| b.block_type == BlockType::SectionHeader).count(), 3);
        // Image pages are sized in pixels.
        assert_eq!(resp.pages[0].width, Some(1240.0));
        assert!(blocks.iter().all(|b| b.bbox.is_some()));
        // Reading order follows body.children.
        assert_eq!(blocks[0].text.as_deref(), Some("Notes on Coastal Erosion"));

        let text = normalize(&fixture("headings"), OutputFormat::Text, "default", None).unwrap();
        assert!(text.pages[0].markdown.starts_with("Notes on Coastal Erosion"));
    }

    #[test]
    fn page_selection_filters_pages() {
        let r = [(2u32, Some(2u32))];
        let resp = normalize(&fixture("multipage"), OutputFormat::Markdown, "default", Some(&r)).unwrap();
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.pages[0].page_number, 2);
    }

    #[test]
    fn failures_surface_docling_errors() {
        let v = json!({"status": "failure", "errors": [{"component_type": "document_backend", "module_name": "x", "error_message": "Input document is not valid."}], "document": {}});
        let e = normalize(&v, OutputFormat::Markdown, "default", None).unwrap_err();
        assert!(e.message.contains("Input document is not valid."), "{e}");
        let e = normalize(&json!({"status": "success", "document": {}}), OutputFormat::Markdown, "default", None)
            .unwrap_err();
        assert!(e.message.contains("json_content"));
        let s = json!({"task_status": "failure", "error_message": "boom"});
        assert_eq!(task_failure_message(&s), "boom");
    }

    #[test]
    fn converts_both_coordinate_origins() {
        let bl = docling_bbox(
            &json!({"l": 10.0, "t": 90.0, "r": 50.0, "b": 70.0, "coord_origin": "BOTTOMLEFT"}),
            100.0,
            100.0,
        )
        .unwrap();
        assert_eq!((bl.x0, bl.y0, bl.x1, bl.y1), (0.1, 0.1, 0.5, 0.3));
        let tl =
            docling_bbox(&json!({"l": 10.0, "t": 10.0, "r": 50.0, "b": 30.0, "coord_origin": "TOPLEFT"}), 100.0, 100.0)
                .unwrap();
        assert_eq!(bl, tl);
        assert!(docling_bbox(&json!({"l": 1.0}), 100.0, 100.0).is_none());
    }

    #[test]
    fn tables_from_cells_and_spanning_items() {
        let data = json!({"num_rows": 2, "num_cols": 2, "table_cells": [
            {"text": "A|B", "start_row_offset_idx": 0, "end_row_offset_idx": 1, "start_col_offset_idx": 0, "end_col_offset_idx": 2},
            {"text": "1", "start_row_offset_idx": 1, "end_row_offset_idx": 2, "start_col_offset_idx": 0, "end_col_offset_idx": 1}
        ]});
        assert_eq!(rows_to_markdown(&table_rows(&data)), "| A\\|B | A\\|B |\n|---|---|\n| 1 |  |");

        let doc = json!({"status": "success", "document": {"json_content": {
        "pages": {"1": {"size": {"width": 100.0, "height": 100.0}, "page_no": 1}, "2": {"size": {"width": 100.0, "height": 100.0}, "page_no": 2}},
        "body": {"children": [{"$ref": "#/texts/0"}, {"$ref": "#/groups/0"}]},
        "furniture": {"children": [{"$ref": "#/texts/1"}]},
        "groups": [{"label": "inline", "children": [{"$ref": "#/texts/2"}, {"$ref": "#/texts/3"}]}],
        "texts": [
            {"label": "text", "text": "abcdef", "prov": [
                {"page_no": 1, "bbox": {"l": 0, "t": 10, "r": 10, "b": 0, "coord_origin": "BOTTOMLEFT"}, "charspan": [0, 3]},
                {"page_no": 2, "bbox": {"l": 0, "t": 100, "r": 10, "b": 90, "coord_origin": "BOTTOMLEFT"}, "charspan": [3, 6]}]},
            {"label": "page_header", "text": "Header", "prov": [{"page_no": 2, "bbox": {"l": 0, "t": 99, "r": 10, "b": 95}}]},
            {"label": "text", "text": "run one", "prov": [{"page_no": 2, "bbox": {"l": 0, "t": 50, "r": 10, "b": 40}}]},
            {"label": "text", "text": "run two", "prov": [{"page_no": 2, "bbox": {"l": 0, "t": 50, "r": 10, "b": 40}}]}
        ]}}});
        let resp = normalize(&doc, OutputFormat::Markdown, "default", None).unwrap();
        assert_eq!(resp.pages[0].markdown, "abc");
        let p2: Vec<&str> = resp.pages[1].blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(p2, ["Header", "def", "run one run two"], "header first, split item, merged inline run");
        assert_eq!(resp.pages[1].blocks[0].block_type, BlockType::Header);
    }

    #[tokio::test]
    async fn request_body_shape() {
        let req = DocumentRequest::from_bytes(&b"%PDF-1.4"[..], "a.pdf")
            .pages("2-3,5")
            .language("de")
            .provider_options(json!({"table_mode": "fast", "to_formats": ["json", "md"]}));
        let dl = Deadline::new(5.0);
        let body = request_body(&req, Some(&crate::util::parse_page_ranges("2-3,5").unwrap()), &dl).await.unwrap();
        assert_eq!(body["sources"][0]["kind"], "file");
        assert_eq!(body["sources"][0]["base64_string"], "JVBERi0xLjQ=");
        assert_eq!(body["sources"][0]["filename"], "a.pdf");
        assert_eq!(body["options"]["page_range"], json!([2, 5]));
        assert_eq!(body["options"]["ocr_lang"], json!(["de"]));
        assert_eq!(body["options"]["table_mode"], "fast");
        assert_eq!(body["options"]["to_formats"], json!(["json", "md"]));
        // URLs are fetched in-process through the filtered path, never forwarded as an http source.
        let e = request_body(&DocumentRequest::from_url("http://127.0.0.1/a.pdf"), Some(&[(3, None)]), &dl)
            .await
            .unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Input, "{e}");
    }

    /// Needs a running docling-serve (`DOCLING_BASE_URL`, default http://localhost:5001). Run with:
    /// `cargo test -p puffinparse-core docling -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a running docling-serve"]
    async fn live_parse() {
        let sample =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmark/datasets/synthetic-v1/docs/multipage_001.pdf");
        let req = DocumentRequest::from_path(sample).model("docling/default").timeout_secs(600.0);
        let resp = Docling.parse(&req, "default").await.expect("docling-serve converts");
        assert_eq!(resp.pages.len(), 2);
        assert!(resp.pages.iter().any(|p| p.blocks.iter().any(|b| b.block_type == BlockType::Table)));
    }
}
