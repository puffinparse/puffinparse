//! Shared plumbing for the vision-LLM providers (`gemini`, `openai`, `anthropic`).
//!
//! A vision LLM is not a layout engine: it receives the whole document inline, is asked for a
//! fixed `{"pages":[{"page_number":1,"markdown":"…"}]}` JSON answer (or the caller's schema in
//! `extract` mode), and returns text without geometry. Everything that does not depend on the
//! vendor's wire format lives here:
//!
//! * document loading (path / bytes / URL download) and encoding (base64, `data:` URLs, MIME
//!   sniffing, a best-effort PDF page count);
//! * the transcription and extraction prompts;
//! * the strict `pages` schema and the strict-structured-output schema rewrite;
//! * turning the model's JSON answer into pages with one geometry-free `text` block each;
//! * token-priced cost and the common response assembly.
//!
//! The vendor modules keep only what differs: endpoints, auth headers, how the document part is
//! shaped, how structured output is requested, and how the answer is dug out of the response.

use crate::error::Result;
use crate::http::{Deadline, Retry};
use crate::provider;
use crate::types::{
    Block, BlockType, DocumentInput, DocumentRequest, ExtractRequest, ExtractResponse, OutputFormat, Page,
    ParseResponse, Usage,
};
use serde_json::{json, Value};

// ---- encoding ------------------------------------------------------------------------------------

/// Standard base64 (with padding). Small and dependency-free — the crate vendors no base64.
pub(crate) fn base64_encode(input: &[u8]) -> String {
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

/// `data:<mime>;base64,<payload>`.
pub(crate) fn data_url(mime: &str, data: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64_encode(data))
}

/// Trust the magic bytes over the extension: some APIs reject a wrong MIME type outright.
pub(crate) fn sniff_mime(data: &[u8], guessed: &str) -> String {
    let sniffed = if data.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    };
    match sniffed {
        Some(m) => m.to_string(),
        None if guessed.is_empty() || guessed == "application/octet-stream" => "text/plain".to_string(),
        None => guessed.to_string(),
    }
}

/// Count `/Type /Page` objects in a PDF (ignoring `/Pages` nodes). Best effort: it misses pages in
/// object streams of compressed PDFs, so it is only ever used as a sanity check / fallback.
pub(crate) fn pdf_page_count(data: &[u8]) -> Option<u32> {
    let needle = b"/Type";
    let mut count = 0u32;
    let mut i = 0usize;
    while i + needle.len() < data.len() {
        if &data[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        while j < data.len() && (data[j] == b' ' || data[j] == b'\r' || data[j] == b'\n' || data[j] == b'\t') {
            j += 1;
        }
        if data[j..].starts_with(b"/Page") {
            let after = data.get(j + 5).copied();
            // `/Page` followed by a delimiter — not `/Pages`.
            if !matches!(after, Some(b's') | Some(b'A'..=b'Z') | Some(b'a'..=b'z') | Some(b'0'..=b'9')) {
                count += 1;
            }
        }
        i = j.max(i + 1);
    }
    (count > 0).then_some(count)
}

/// Defensive: structured output should never be fenced, but prompts can be overridden.
pub(crate) fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else { return t };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches(['\r', '\n']).trim_end().trim_end_matches('`').trim_end()
}

// ---- document loading ----------------------------------------------------------------------------

/// Read the document bytes and decide its MIME type. URLs are downloaded (through the
/// address-filtered [`crate::fetch`] path) and inlined — the vision APIs do not fetch a URL that
/// needs the caller's credentials.
pub(crate) async fn load_document(
    request: &DocumentRequest,
    deadline: &Deadline,
    retry: Retry,
    provider_name: &'static str,
) -> Result<(bytes::Bytes, String)> {
    if let Some(data) = provider::load_bytes(&request.input).await? {
        return Ok((data, request.input.mime_type()));
    }
    let DocumentInput::Url { url } = &request.input else {
        unreachable!("load_bytes only returns None for URL inputs");
    };
    let fetched = crate::fetch::fetch_document(provider_name, url, deadline, retry).await?;
    let mime = fetched.mime_or(request.input.mime_type());
    let data = fetched.data;
    Ok((data, mime))
}

// ---- prompts and schemas -------------------------------------------------------------------------

/// How the model hands its answer back, which changes a line or two of the prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Structured output: the whole reply is the JSON object (OpenAI `text.format`).
    Json,
    /// A forced tool call whose input is the JSON object (Anthropic `tool_choice`).
    Tool,
}

/// The strict-compatible `parse` schema: every object closed and fully `required`.
pub(crate) fn pages_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pages": {
                "type": "array",
                "description": "Every page of the document, in order.",
                "items": {
                    "type": "object",
                    "properties": {
                        "page_number": {"type": "integer", "description": "1-based page number in the document."},
                        "markdown": {"type": "string", "description": "The full content of the page."},
                    },
                    "required": ["page_number", "markdown"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["pages"],
        "additionalProperties": false,
    })
}

/// The transcription prompt for `parse` mode.
pub(crate) fn parse_prompt(request: &DocumentRequest, answer: Answer) -> String {
    let mut p = String::from(
        "Transcribe this document. Return one entry in `pages` for every page, in order.\n\n\
         Rules:\n\
         - Use the document's own 1-based page numbers.\n\
         - `markdown` holds the complete content of that page as GitHub-Flavored Markdown: `#` headings, \
         `-` lists, GFM pipe tables, fenced code, LaTeX between `$` for formulas.\n\
         - Transcribe verbatim: same wording, numbers, casing and reading order (top to bottom; \
         column by column on multi-column pages). Never summarise, translate, explain or invent content.\n\
         - Describe a figure, photo or chart as one short italic line, e.g. \
         `*Figure: bar chart of quarterly revenue.*`\n\
         - Keep running headers, footers and printed page numbers where they appear.\n\
         - A blank page gets an empty `markdown` string.",
    );
    if answer == Answer::Json {
        p.push_str("\n- Return only the JSON object the schema asks for; no prose, no code fences around it.");
    }
    if request.output == OutputFormat::Text {
        p.push_str("\n- Exception: emit plain text only — no Markdown syntax, no table pipes, no `#`.");
    }
    if let Some(pages) = &request.pages {
        p.push_str(&format!(
            "\n- Transcribe only pages {pages} (1-based, inclusive ranges). Skip every other page and keep \
             the original page numbers for the ones you return."
        ));
    }
    if let Some(language) = &request.language {
        p.push_str(&format!("\n- The document is mainly in {language}; transcribe it in that language."));
    }
    p
}

/// The extraction prompt for `extract` mode (the schema itself travels as structured output).
pub(crate) fn extract_prompt(request: &ExtractRequest, answer: Answer) -> String {
    let mut p = String::from(match answer {
        Answer::Json => {
            "Extract the requested fields from this document and return them as JSON matching the schema.\n\n"
        }
        Answer::Tool => "Extract the requested fields from this document and record them with the tool.\n\n",
    });
    p.push_str(
        "Rules:\n\
         - Use only values that appear in the document; never guess, infer or fabricate.\n\
         - Use `null` for a value the document does not contain.\n\
         - Numbers must be plain numbers (no thousands separators, no currency symbols); keep dates as \
         written unless the schema says otherwise.",
    );
    if answer == Answer::Json {
        p.push_str("\n- Return only the JSON object the schema asks for.");
    }
    if let Some(pages) = &request.document.pages {
        p.push_str(&format!("\n- Only look at pages {pages} (1-based, inclusive ranges)."));
    }
    if let Some(language) = &request.document.language {
        p.push_str(&format!("\n- The document is mainly in {language}."));
    }
    if let Some(instructions) = &request.instructions {
        p.push_str("\n\nAdditional instructions from the caller:\n");
        p.push_str(instructions);
    }
    p
}

/// Make a schema satisfy strict structured outputs / strict tool use: every object must set
/// `additionalProperties: false` and list **all** of its properties in `required`.
pub(crate) fn sanitize_strict_schema(schema: &mut Value) {
    const SCHEMA_MAPS: &[&str] = &["properties", "$defs", "definitions", "patternProperties"];
    const SCHEMA_NODES: &[&str] =
        &["items", "prefixItems", "anyOf", "oneOf", "allOf", "not", "if", "then", "else", "contains"];

    let Value::Object(map) = schema else { return };
    for key in SCHEMA_MAPS {
        if let Some(Value::Object(children)) = map.get_mut(*key) {
            for child in children.values_mut() {
                sanitize_strict_schema(child);
            }
        }
    }
    for key in SCHEMA_NODES {
        match map.get_mut(*key) {
            Some(Value::Array(items)) => items.iter_mut().for_each(sanitize_strict_schema),
            Some(child) if child.is_object() => sanitize_strict_schema(child),
            _ => {}
        }
    }
    let is_object = match map.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("object")),
        _ => map.contains_key("properties"),
    };
    if is_object {
        map.insert("additionalProperties".to_string(), json!(false));
        let required: Vec<Value> = map
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().map(|k| Value::String(k.clone())).collect())
            .unwrap_or_default();
        map.insert("required".to_string(), Value::Array(required));
    }
}

// ---- provider_options ------------------------------------------------------------------------------

/// Keys of `provider_options` PuffinParse consumes instead of forwarding to the provider.
pub(crate) const PRIVATE_OPTIONS: &[&str] = &["strict"];

/// `provider_options.strict`, defaulting per provider.
pub(crate) fn strict_enabled(request: &DocumentRequest, default: bool) -> bool {
    request.option("strict").and_then(Value::as_bool).unwrap_or(default)
}

/// `provider_options` minus PuffinParse's own keys, or `None` when nothing is left to forward.
pub(crate) fn passthrough_options(request: &DocumentRequest) -> Option<Value> {
    let mut opts = request.provider_options.clone()?;
    if let Value::Object(map) = &mut opts {
        for key in PRIVATE_OPTIONS {
            map.remove(*key);
        }
        if map.is_empty() {
            return None;
        }
    }
    Some(opts)
}

// ---- normalisation -----------------------------------------------------------------------------------

/// One page holding exactly one geometry-free `text` block: what a vision LLM can honestly report.
pub(crate) fn text_page(page_number: u32, content: String, text: String) -> Page {
    let block = Block {
        block_type: BlockType::Text,
        content: content.clone(),
        text: Some(text.clone()),
        bbox: None,
        confidence: None,
        page_number,
    };
    Page { page_number, width: None, height: None, markdown: content, text, blocks: vec![block] }
}

/// A usable 1-based page number, or the entry's ordinal.
pub(crate) fn page_number_or(value: Option<u64>, index: usize) -> u32 {
    value.and_then(|n| u32::try_from(n).ok()).filter(|&n| n > 0).unwrap_or(index as u32 + 1)
}

/// Build pages (one text block each, no geometry) from `{"pages":[{"page_number","markdown"}]}`.
pub(crate) fn pages_from_value(data: &Value, fmt: OutputFormat) -> Vec<Page> {
    let entries = data.get("pages").and_then(Value::as_array).cloned().unwrap_or_default();
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let page_number = page_number_or(entry.get("page_number").and_then(Value::as_u64), i);
            let content = entry.get("markdown").and_then(Value::as_str).unwrap_or_default().trim().to_string();
            let text = match fmt {
                OutputFormat::Markdown => crate::types::markdown_to_text(&content),
                OutputFormat::Text => content.clone(),
            };
            text_page(page_number, content, text)
        })
        .collect()
}

/// Turn the model's `pages` answer into a [`ParseResponse`] (usage = returned page count).
pub(crate) fn normalize_parse(provider_name: &str, data: &Value, fmt: OutputFormat, model: &str) -> ParseResponse {
    let pages = pages_from_value(data, fmt);
    let usage = Usage { pages: pages.len() as u32, credits: None, provider_cost_usd: None };
    ParseResponse::from_pages(provider_name, &format!("{provider_name}/{model}"), pages, usage)
}

// ---- token pricing and response assembly ----------------------------------------------------------

/// Dollar cost of one call from a `(model, input $/1M, output $/1M)` price table.
pub(crate) fn token_cost(
    prices: &[(&str, f64, f64)],
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    let (_, input, output) = prices.iter().find(|(m, _, _)| *m == model)?;
    Some((input_tokens as f64) * input / 1e6 + (output_tokens as f64) * output / 1e6)
}

/// What one request/response round-trip to a chat-style vision API yields.
#[derive(Debug)]
pub(crate) struct Completion {
    pub id: String,
    pub data: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub raw: Value,
}

impl Completion {
    /// Read `id` and `usage.{input,output}_tokens` (the OpenAI and Anthropic spelling) from `raw`.
    pub(crate) fn new(raw: Value, data: Value) -> Self {
        let id = raw.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let usage = raw.get("usage");
        let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
        let (input_tokens, output_tokens) = (tokens("input_tokens"), tokens("output_tokens"));
        Self { id, data, input_tokens, output_tokens, raw }
    }
}

/// Assemble a `parse` response: pages, exact token cost, job id and token metadata.
pub(crate) fn parse_response(
    provider_name: &str,
    prices: &[(&str, f64, f64)],
    request: &DocumentRequest,
    model: &str,
    call: Completion,
) -> ParseResponse {
    let mut resp = normalize_parse(provider_name, &call.data, request.output, model);
    resp.usage.provider_cost_usd = token_cost(prices, model, call.input_tokens, call.output_tokens);
    resp.provider_job_id = Some(call.id);
    resp.metadata.insert(format!("{provider_name}_input_tokens"), json!(call.input_tokens));
    resp.metadata.insert(format!("{provider_name}_output_tokens"), json!(call.output_tokens));
    if request.include_raw {
        resp.raw = Some(call.raw);
    }
    resp
}

/// Assemble an `extract` response. The provider reports tokens, never pages: `usage.pages` stays
/// 0 and the cost comes from the token counts. Vision LLMs have no per-field grounding, so
/// `fields` stays empty and a citation request is flagged in metadata.
pub(crate) fn extract_response(
    provider_name: &str,
    prices: &[(&str, f64, f64)],
    request: &ExtractRequest,
    model: &str,
    call: Completion,
) -> ExtractResponse {
    let usage = Usage {
        pages: 0,
        credits: None,
        provider_cost_usd: token_cost(prices, model, call.input_tokens, call.output_tokens),
    };
    let mut resp = ExtractResponse::new(provider_name, &format!("{provider_name}/{model}"), call.data, usage);
    resp.provider_job_id = Some(call.id);
    resp.metadata.insert(format!("{provider_name}_input_tokens"), json!(call.input_tokens));
    resp.metadata.insert(format!("{provider_name}_output_tokens"), json!(call.output_tokens));
    if request.citations {
        resp.metadata.insert(format!("{provider_name}_citations_unsupported"), json!(true));
    }
    if request.document.include_raw {
        resp.raw = Some(call.raw);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_data_url() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(data_url("image/png", b"fo"), "data:image/png;base64,Zm8=");
    }

    #[test]
    fn prompts_differ_only_in_the_answer_channel() {
        let req = DocumentRequest::from_path("a.pdf");
        let json = parse_prompt(&req, Answer::Json);
        let tool = parse_prompt(&req, Answer::Tool);
        assert!(json.contains("no code fences") && !tool.contains("no code fences"));
        assert!(json.starts_with(&tool));
        let e = ExtractRequest::new(req, json!({}));
        assert!(extract_prompt(&e, Answer::Json).contains("as JSON matching the schema"));
        assert!(extract_prompt(&e, Answer::Tool).contains("record them with the tool"));
    }

    #[test]
    fn responses_carry_tokens_cost_and_citation_flag() {
        const PRICES: &[(&str, f64, f64)] = &[("m", 1.0, 2.0)];
        let raw = json!({"id": "r1", "usage": {"input_tokens": 1000, "output_tokens": 500}});
        let data = json!({"pages": [{"page_number": 1, "markdown": "# A"}]});
        let req = DocumentRequest::from_path("a.pdf");
        let resp = parse_response("p", PRICES, &req, "m", Completion::new(raw.clone(), data));
        assert_eq!(resp.model, "p/m");
        assert_eq!(resp.provider_job_id.as_deref(), Some("r1"));
        assert_eq!(resp.metadata["p_input_tokens"], 1000);
        assert!((resp.usage.provider_cost_usd.unwrap() - 0.002).abs() < 1e-12);
        assert!(resp.raw.is_none());

        let ereq = ExtractRequest::new(req.include_raw(true), json!({})).citations(true);
        let resp = extract_response("p", PRICES, &ereq, "m", Completion::new(raw, json!({"a": 1})));
        assert_eq!(resp.usage.pages, 0);
        assert_eq!(resp.metadata["p_citations_unsupported"], true);
        assert!(resp.raw.is_some());
        assert_eq!(token_cost(PRICES, "other", 1, 1), None);
    }
}
