//! The MCP tools: `parse`, `ocr`, `extract`, `list_models`, `compare`.
//!
//! Each tool validates its arguments, resolves the model against the registry and the server's
//! allow-list *before* any network call, then calls the same core entry points as the CLI. Provider
//! and input failures come back as tool results with `isError: true` (the model can correct the
//! call); only malformed protocol messages become JSON-RPC errors (see `mod.rs`).
//!
//! Credentials are read by the core from the provider's environment variable. No tool accepts an
//! API key or a base URL, so a prompt-injected document cannot redirect a call or its key.

use super::Features;
use puffinparse_core::{self as core, DocumentRequest, ErrorKind, ExtractRequest, Mode, ModelRef, OutputFormat};
use serde_json::{json, Map, Value};
use std::time::Instant;

pub const NAMES: &[&str] = &["parse", "ocr", "extract", "list_models", "compare"];

/// Characters of document text returned by `parse` / `ocr` unless `max_chars` says otherwise.
const DEFAULT_MAX_CHARS: usize = 40_000;
const MAX_MAX_CHARS: usize = 400_000;
const DEFAULT_EXCERPT_CHARS: usize = 1_500;
const MAX_EXCERPT_CHARS: usize = 10_000;
const MAX_COMPARE_MODELS: usize = 8;
const MAX_BLOCKS: usize = 500;
const BLOCK_EXCERPT_CHARS: usize = 160;

const SENT_TO_PROVIDER: &str = "The document is sent to the provider of the chosen model (a local engine such as \
tesseract or docling keeps it on your machine or server) and the provider bills the user at its per-page price; \
list_models shows prices and which providers have a key configured.";

// ---- configuration -------------------------------------------------------------------------------

/// Server configuration (from `puffinparse mcp` flags).
#[derive(Debug, Clone)]
pub struct Config {
    pub allow: AllowList,
    /// Whole-call deadline per provider call, seconds.
    pub timeout_secs: f64,
    pub max_retries: u32,
    /// `--root`: when set (canonical), local inputs must resolve inside this directory.
    pub root: Option<std::path::PathBuf>,
    /// Test-only provider endpoints: model (`provider/model`) or provider name → (base URL, key).
    #[cfg(test)]
    pub endpoints: std::collections::BTreeMap<String, (String, String)>,
}

impl Config {
    pub fn new(allow: AllowList, timeout_secs: f64, max_retries: u32, root: Option<std::path::PathBuf>) -> Self {
        Self {
            allow,
            timeout_secs,
            max_retries,
            root,
            #[cfg(test)]
            endpoints: Default::default(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allow: AllowList::default(),
            timeout_secs: 300.0,
            max_retries: 2,
            root: None,
            #[cfg(test)]
            endpoints: Default::default(),
        }
    }
}

/// `--models`: which models the server may call. Empty means every model.
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    /// Normalized patterns: `provider/model` or `provider/*`.
    patterns: Vec<String>,
}

impl AllowList {
    /// Accepts `provider/model`, `provider/*`, a bare `provider` (same as `provider/*`) or `*`.
    /// Provider aliases are canonicalized (`llama` → `llamaparse`); unknown names are rejected so
    /// a typo cannot silently disable a model.
    pub fn new(patterns: &[String]) -> Result<Self, core::Error> {
        let mut out = Vec::new();
        for raw in patterns {
            let p = raw.trim();
            if p.is_empty() {
                continue;
            }
            if p == "*" {
                return Ok(Self::default());
            }
            let normalized = match p.split_once('/') {
                Some((prov, "*")) => format!("{}/*", ModelRef::parse(prov)?.provider),
                Some(_) => ModelRef::parse(p)?.qualified(),
                None => format!("{}/*", ModelRef::parse(p)?.provider),
            };
            if !out.contains(&normalized) {
                out.push(normalized);
            }
        }
        Ok(Self { patterns: out })
    }

    pub fn allows(&self, qualified: &str) -> bool {
        self.patterns.is_empty()
            || self.patterns.iter().any(|p| match p.strip_suffix("/*") {
                Some(prov) => qualified.split_once('/').is_some_and(|(q, _)| q == prov),
                None => p == qualified,
            })
    }

    pub fn describe(&self) -> String {
        if self.patterns.is_empty() {
            "all models".into()
        } else {
            self.patterns.join(", ")
        }
    }

    fn patterns(&self) -> Option<&[String]> {
        (!self.patterns.is_empty()).then_some(self.patterns.as_slice())
    }
}

// ---- results -------------------------------------------------------------------------------------

/// One `tools/call` result.
#[derive(Debug)]
pub struct ToolOutput {
    texts: Vec<String>,
    structured: Option<Value>,
    is_error: bool,
}

impl ToolOutput {
    fn ok(texts: Vec<String>, structured: Value) -> Self {
        Self { texts, structured: Some(structured), is_error: false }
    }

    fn error(message: impl Into<String>) -> Self {
        Self { texts: vec![message.into()], structured: None, is_error: true }
    }

    pub(crate) fn to_json(&self, features: Features) -> Value {
        let content: Vec<Value> = self.texts.iter().map(|t| json!({ "type": "text", "text": t })).collect();
        let mut v = json!({ "content": content, "isError": self.is_error });
        if features.structured {
            if let Some(s) = &self.structured {
                v["structuredContent"] = s.clone();
            }
        }
        v
    }
}

/// Run one tool. Never fails at the protocol level: every problem is an `isError` result.
pub async fn call(cfg: &Config, name: &str, args: &Map<String, Value>) -> ToolOutput {
    let result = match name {
        "parse" => parse(cfg, args).await,
        "ocr" => ocr(cfg, args).await,
        "extract" => extract(cfg, args).await,
        "list_models" => list_models(cfg, args),
        "compare" => compare(cfg, args).await,
        other => Err(format!("unknown tool '{other}'")),
    };
    result.unwrap_or_else(ToolOutput::error)
}

// ---- argument helpers ----------------------------------------------------------------------------

struct Args<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    /// Reject unknown argument names, so a typo (`page` for `pages`) is reported, not ignored.
    fn new(map: &'a Map<String, Value>, accepted: &[&str]) -> Result<Self, String> {
        if let Some(k) = map.keys().find(|k| !accepted.contains(&k.as_str())) {
            return Err(format!("unknown argument '{k}' (accepted: {})", accepted.join(", ")));
        }
        Ok(Self { map })
    }

    fn get(&self, k: &str) -> Option<&'a Value> {
        self.map.get(k).filter(|v| !v.is_null())
    }

    fn string(&self, k: &str) -> Result<Option<String>, String> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
            Some(_) => Err(format!("argument '{k}' must be a string")),
        }
    }

    fn required_string(&self, k: &str) -> Result<String, String> {
        self.string(k)?.ok_or_else(|| format!("argument '{k}' is required"))
    }

    fn bool(&self, k: &str) -> Result<Option<bool>, String> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("argument '{k}' must be a boolean")),
        }
    }

    fn count(&self, k: &str, default: usize, max: usize) -> Result<usize, String> {
        match self.get(k) {
            None => Ok(default),
            Some(v) => match v.as_u64() {
                Some(n) if n >= 1 => Ok((n as usize).min(max)),
                _ => Err(format!("argument '{k}' must be a positive integer")),
            },
        }
    }
}

/// `file`: a local path, a `file://` URL, or an http(s) URL. Other schemes are refused here
/// rather than being read as a (nonexistent) relative path. With `--root`, a local path is
/// canonicalized (symlinks followed) and must lie inside the root; the canonical path is what the
/// provider call then reads.
fn input(cfg: &Config, file: &str) -> Result<String, String> {
    if file.starts_with("http://") || file.starts_with("https://") {
        return Ok(file.to_string());
    }
    let path = match file.strip_prefix("file://") {
        Some(p) => p,
        None => {
            if let Some((scheme, _)) = file.split_once("://") {
                if !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
                    return Err(format!("unsupported URL scheme '{scheme}': pass a local file path or an http(s) URL"));
                }
            }
            file
        }
    };
    let Some(root) = &cfg.root else { return Ok(path.to_string()) };
    let canonical = std::fs::canonicalize(path).map_err(|e| format!("input_error: cannot read {path}: {e}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "'{path}' is outside the directory this server may read ({}); the server was started with --root. \
             Pass a file inside it.",
            root.display()
        ));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

/// Resolve a model string for `mode` and check it against the allow-list. No network.
fn resolve_model(cfg: &Config, model: &str, mode: Mode) -> Result<String, String> {
    let qualified = ModelRef::parse_for(model, mode).map_err(|e| error_text(&e))?.qualified();
    if !cfg.allow.allows(&qualified) {
        return Err(format!(
            "model '{qualified}' is not enabled on this server (enabled: {}). Call list_models to see the enabled models.",
            cfg.allow.describe()
        ));
    }
    Ok(qualified)
}

/// Build the core request for one model. `model` must already be resolved.
fn document_request(
    cfg: &Config,
    file: &str,
    model: &str,
    pages: Option<&str>,
    language: Option<&str>,
) -> DocumentRequest {
    let mut req =
        DocumentRequest::from_str_input(file).model(model).timeout_secs(cfg.timeout_secs).max_retries(cfg.max_retries);
    if let Some(p) = pages {
        req = req.pages(p);
    }
    if let Some(l) = language {
        req = req.language(l);
    }
    #[cfg(test)]
    {
        let provider = model.split('/').next().unwrap_or(model);
        if let Some((base, key)) = cfg.endpoints.get(model).or_else(|| cfg.endpoints.get(provider)) {
            req = req.base_url(base.clone()).api_key(key.clone());
        }
    }
    req
}

/// A core error as tool-result text: the typed kind, provider, HTTP status and the provider's own
/// message (via `Display`), plus a hint where one helps the model recover.
pub fn error_text(e: &core::Error) -> String {
    let mut s = e.to_string();
    let hint = match e.kind {
        ErrorKind::Authentication => {
            e.provider.as_deref().and_then(core::model::provider_info).filter(|p| !p.env_var.is_empty()).map(|p| {
                format!(
                    "Set {} in the environment the MCP server is started from, then restart it. Keys are never \
                     accepted as tool arguments.",
                    p.env_var
                )
            })
        }
        ErrorKind::UnsupportedModel => Some("Call list_models to see the valid model strings for each mode.".into()),
        ErrorKind::RateLimit => Some("The provider is rate limiting; wait and retry, or choose another model.".into()),
        ErrorKind::Timeout => {
            Some("Use `pages` to process fewer pages, or start the server with a larger --timeout.".into())
        }
        _ => None,
    };
    if let Some(h) = hint {
        s.push_str(". ");
        s.push_str(&h);
    }
    s
}

fn error_json(e: &core::Error) -> Value {
    json!({
        "kind": e.kind,
        "message": error_text(e),
        "provider": e.provider,
        "status_code": e.status_code,
        "retryable": e.retryable,
    })
}

/// Cut `s` to at most `max` characters (on a char boundary). Returns the kept text and whether
/// anything was dropped.
fn truncate(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

fn truncation_note(shown: usize, total: usize) -> String {
    format!(
        "\n\n[truncated by puffinparse: showing the first {shown} of {total} characters. Call again with `pages` \
         to read the rest, or raise `max_chars`.]"
    )
}

/// Everything in `structured` except `body_key`, as compact JSON for the second text block
/// (clients that ignore `structuredContent` still see the numbers).
fn metadata_text(structured: &Value, body_key: &str) -> String {
    let mut meta = structured.clone();
    if let Value::Object(m) = &mut meta {
        m.remove(body_key);
    }
    meta.to_string()
}

// ---- parse / ocr ---------------------------------------------------------------------------------

async fn parse(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["file", "model", "pages", "output", "include_blocks", "language", "max_chars"])?;
    let file = input(cfg, &a.required_string("file")?)?;
    let model = resolve_model(cfg, &a.required_string("model")?, Mode::Parse)?;
    let output = match a.string("output")?.as_deref() {
        None | Some("markdown") => OutputFormat::Markdown,
        Some("text") => OutputFormat::Text,
        Some(o) => return Err(format!("argument 'output' must be \"markdown\" or \"text\", not \"{o}\"")),
    };
    let include_blocks = a.bool("include_blocks")?.unwrap_or(false);
    let max_chars = a.count("max_chars", DEFAULT_MAX_CHARS, MAX_MAX_CHARS)?;
    let req = document_request(cfg, &file, &model, a.string("pages")?.as_deref(), a.string("language")?.as_deref())
        .output(output);

    let resp = match core::parse(req).await {
        Ok(r) => r,
        Err(e) => return Ok(ToolOutput::error(error_text(&e))),
    };
    let body = if output == OutputFormat::Text { &resp.text } else { &resp.markdown };
    let total = body.chars().count();
    let (shown, truncated) = truncate(body, max_chars);
    let mut structured = json!({
        "model": resp.model,
        "provider": resp.provider,
        "pages": resp.usage.pages,
        "latency_ms": resp.latency_ms,
        "cost_usd": resp.cost_usd,
        "output": if output == OutputFormat::Text { "text" } else { "markdown" },
        "total_chars": total,
        "truncated": truncated,
    });
    if include_blocks {
        let (blocks, n) = blocks_summary(&resp);
        structured["blocks"] = blocks;
        structured["blocks_total"] = json!(n);
    }
    let mut text = if total == 0 { "(the provider returned no text for this document)".to_string() } else { shown };
    if truncated {
        text.push_str(&truncation_note(max_chars, total));
    }
    // The body goes in both forms: clients that read `structuredContent` (Claude Code does, and then
    // hides the text blocks) need it there, and clients without structured output read the text.
    structured["content"] = json!(text);
    let meta = metadata_text(&structured, "content");
    Ok(ToolOutput::ok(vec![text, meta], structured))
}

/// Per-block summary (type, page, box, confidence, a short excerpt), capped at [`MAX_BLOCKS`].
fn blocks_summary(resp: &core::ParseResponse) -> (Value, usize) {
    let all: Vec<&core::Block> = resp.pages.iter().flat_map(|p| p.blocks.iter()).collect();
    let blocks: Vec<Value> = all
        .iter()
        .take(MAX_BLOCKS)
        .map(|b| {
            let (excerpt, _) = truncate(&b.content, BLOCK_EXCERPT_CHARS);
            json!({
                "type": b.block_type.as_str(),
                "page": b.page_number,
                "bbox": b.bbox.as_ref().map(|bb| [bb.x0, bb.y0, bb.x1, bb.y1]),
                "confidence": b.confidence,
                "chars": b.content.chars().count(),
                "excerpt": excerpt,
            })
        })
        .collect();
    (Value::Array(blocks), all.len())
}

async fn ocr(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["file", "model", "pages", "language", "max_chars"])?;
    let file = input(cfg, &a.required_string("file")?)?;
    let model = resolve_model(cfg, &a.required_string("model")?, Mode::Ocr)?;
    let max_chars = a.count("max_chars", DEFAULT_MAX_CHARS, MAX_MAX_CHARS)?;
    let req = document_request(cfg, &file, &model, a.string("pages")?.as_deref(), a.string("language")?.as_deref())
        .output(OutputFormat::Text);

    let resp = match core::ocr(req).await {
        Ok(r) => r,
        Err(e) => return Ok(ToolOutput::error(error_text(&e))),
    };
    let total = resp.text.chars().count();
    let (shown, truncated) = truncate(&resp.text, max_chars);
    let derived = resp.metadata.get("puffinparse_derived_from").and_then(Value::as_str) == Some("parse");
    let mut structured = json!({
        "model": resp.model,
        "provider": resp.provider,
        "pages": resp.usage.pages,
        "latency_ms": resp.latency_ms,
        "cost_usd": resp.cost_usd,
        "derived_from_parse": derived,
        "total_chars": total,
        "truncated": truncated,
    });
    let mut text = if total == 0 { "(the provider returned no text for this document)".to_string() } else { shown };
    if truncated {
        text.push_str(&truncation_note(max_chars, total));
    }
    // The body goes in both forms: clients that read `structuredContent` (Claude Code does, and then
    // hides the text blocks) need it there, and clients without structured output read the text.
    structured["content"] = json!(text);
    let meta = metadata_text(&structured, "content");
    Ok(ToolOutput::ok(vec![text, meta], structured))
}

// ---- extract -------------------------------------------------------------------------------------

async fn extract(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["file", "model", "schema", "instructions", "citations", "pages", "language"])?;
    let file = input(cfg, &a.required_string("file")?)?;
    let model = resolve_model(cfg, &a.required_string("model")?, Mode::Extract)?;
    let schema = match a.get("schema") {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        // Some clients flatten nested objects to strings; accept a JSON-encoded schema too.
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => v,
            _ => return Err("argument 'schema' must be a JSON Schema object".into()),
        },
        Some(_) => return Err("argument 'schema' must be a JSON Schema object".into()),
        None => return Err("argument 'schema' is required".into()),
    };
    let doc = document_request(cfg, &file, &model, a.string("pages")?.as_deref(), a.string("language")?.as_deref());
    let mut req = ExtractRequest::new(doc, schema).citations(a.bool("citations")?.unwrap_or(false));
    if let Some(i) = a.string("instructions")? {
        req = req.instructions(i);
    }

    let resp = match core::extract(req).await {
        Ok(r) => r,
        Err(e) => return Ok(ToolOutput::error(error_text(&e))),
    };
    let structured = json!({
        "model": resp.model,
        "provider": resp.provider,
        "pages": resp.usage.pages,
        "latency_ms": resp.latency_ms,
        "cost_usd": resp.cost_usd,
        "data": resp.data,
        "fields": resp.fields,
    });
    let data = serde_json::to_string_pretty(&resp.data).unwrap_or_else(|_| resp.data.to_string());
    let meta = metadata_text(&structured, "data");
    Ok(ToolOutput::ok(vec![data, meta], structured))
}

// ---- list_models ---------------------------------------------------------------------------------

fn key_configured(env_var: &str) -> bool {
    !env_var.is_empty() && std::env::var(env_var).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

fn list_models(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["mode", "provider", "include_descriptions"])?;
    let include_descriptions = a.bool("include_descriptions")?.unwrap_or(false);
    let mode: Option<Mode> = match a.string("mode")? {
        Some(m) => Some(m.parse().map_err(|e: core::Error| e.message)?),
        None => None,
    };
    let provider = match a.string("provider")? {
        Some(p) => Some(ModelRef::parse(&p).map_err(|e| error_text(&e))?.provider),
        None => None,
    };
    let prices = core::pricing::all_prices();
    let mut models = Vec::new();
    for p in core::PROVIDERS.iter().filter(|p| provider.as_deref().map_or(true, |f| f == p.name)) {
        let configured = key_configured(p.env_var);
        for m in p.models.iter().filter(|m| mode.map_or(true, |md| m.supports(md))) {
            let q = m.qualified();
            if !cfg.allow.allows(&q) {
                continue;
            }
            let per_page: Map<String, Value> = m
                .modes
                .iter()
                .filter_map(|md| {
                    prices.get(&q).and_then(|e| e.for_mode(*md)).map(|c| (md.as_str().to_string(), json!(c)))
                })
                .collect();
            let mut entry = json!({
                "model": q,
                "provider": p.name,
                "modes": m.modes,
                "default": m.default,
                "per_page_usd": per_page,
                "self_hosted": p.self_hosted(),
                "env_var": (!p.env_var.is_empty()).then_some(p.env_var),
                "key_required": p.key_required(),
                "key_configured": configured,
                "ready": !p.key_required() || configured,
            });
            if include_descriptions {
                entry["description"] = json!(m.description);
            }
            models.push(entry);
        }
    }
    let structured = json!({
        "count": models.len(),
        "models": models,
        "allow_list": cfg.allow.patterns(),
        "note": "`ready` means the provider's key is set in this server's environment (or none is needed); \
                 self-hosted engines still need their engine installed or reachable. Default models are \
                 per provider and mode; a bare provider name picks it. Prices are list prices in USD per page.",
    });
    Ok(ToolOutput::ok(vec![structured.to_string()], structured))
}

// ---- compare -------------------------------------------------------------------------------------

async fn compare(cfg: &Config, args: &Map<String, Value>) -> Result<ToolOutput, String> {
    let a = Args::new(args, &["file", "models", "mode", "pages", "language", "excerpt_chars"])?;
    let file = input(cfg, &a.required_string("file")?)?;
    let mode = match a.string("mode")?.as_deref() {
        None | Some("parse") => Mode::Parse,
        Some("ocr") => Mode::Ocr,
        Some(o) => {
            return Err(format!(
                "argument 'mode' must be \"parse\" or \"ocr\", not \"{o}\" (compare extract results by calling extract per model)"
            ))
        }
    };
    let raw_models = match a.get("models") {
        Some(Value::Array(v)) => v,
        Some(_) => return Err("argument 'models' must be an array of model strings".into()),
        None => return Err("argument 'models' is required".into()),
    };
    let mut models: Vec<String> = Vec::new();
    for m in raw_models {
        let m = m.as_str().ok_or("argument 'models' must contain only strings")?;
        let q = resolve_model(cfg, m, mode)?;
        if !models.contains(&q) {
            models.push(q);
        }
    }
    if models.is_empty() {
        return Err("argument 'models' must name at least one model".into());
    }
    if models.len() > MAX_COMPARE_MODELS {
        return Err(format!("compare runs at most {MAX_COMPARE_MODELS} models per call ({} given)", models.len()));
    }
    let excerpt_chars = a.count("excerpt_chars", DEFAULT_EXCERPT_CHARS, MAX_EXCERPT_CHARS)?;
    let pages = a.string("pages")?;
    let language = a.string("language")?;

    let runs = models.iter().map(|model| {
        let req = document_request(cfg, &file, model, pages.as_deref(), language.as_deref());
        async move {
            let started = Instant::now();
            let outcome = match mode {
                Mode::Ocr => core::ocr(req.output(OutputFormat::Text))
                    .await
                    .map(|r| (r.text, r.usage.pages, r.latency_ms, r.cost_usd)),
                _ => core::parse(req).await.map(|r| (r.markdown, r.usage.pages, r.latency_ms, r.cost_usd)),
            };
            (model.clone(), outcome, started.elapsed().as_millis() as u64)
        }
    });
    let outcomes = futures::future::join_all(runs).await;

    let mut results = Vec::new();
    let mut total_cost = None::<f64>;
    let mut table =
        String::from("| model | status | pages | latency ms | est. cost | chars |\n|---|---|---|---|---|---|\n");
    let mut excerpts = String::new();
    for (model, outcome, elapsed) in outcomes {
        match outcome {
            Ok((body, pages, latency_ms, cost)) => {
                let chars = body.chars().count();
                let (excerpt, cut) = truncate(&body, excerpt_chars);
                if let Some(c) = cost {
                    *total_cost.get_or_insert(0.0) += c;
                }
                let cost_s = cost.map(|c| format!("${c:.4}")).unwrap_or_else(|| "n/a".into());
                table.push_str(&format!("| {model} | ok | {pages} | {latency_ms} | {cost_s} | {chars} |\n"));
                excerpts.push_str(&format!(
                    "\n### {model}\n\n{}{}\n",
                    if excerpt.is_empty() { "(no text)" } else { &excerpt },
                    if cut {
                        format!("\n\n[excerpt: first {excerpt_chars} of {chars} characters]")
                    } else {
                        String::new()
                    }
                ));
                results.push(json!({
                    "model": model, "ok": true, "pages": pages, "latency_ms": latency_ms, "cost_usd": cost,
                    "chars": chars, "excerpt": excerpt, "excerpt_truncated": cut, "error": null,
                }));
            }
            Err(e) => {
                table.push_str(&format!("| {model} | error: {} | | {elapsed} | | |\n", e.kind));
                excerpts.push_str(&format!("\n### {model}\n\nError: {}\n", error_text(&e)));
                results.push(json!({
                    "model": model, "ok": false, "pages": null, "latency_ms": elapsed, "cost_usd": null,
                    "chars": null, "excerpt": null, "excerpt_truncated": false, "error": error_json(&e),
                }));
            }
        }
    }
    let succeeded = results.iter().filter(|r| r["ok"] == json!(true)).count();
    let failed = results.len() - succeeded;
    let structured = json!({
        "mode": mode.as_str(),
        "file": file,
        "succeeded": succeeded,
        "failed": failed,
        "total_cost_usd": total_cost,
        "results": results,
    });
    let header = format!(
        "Compared {} model(s) on {file} in {} mode: {succeeded} succeeded, {failed} failed{}.\n\n",
        models.len(),
        mode.as_str(),
        total_cost.map(|c| format!(", est. total ${c:.4}")).unwrap_or_default()
    );
    let mut out = ToolOutput::ok(vec![format!("{header}{table}{excerpts}")], structured);
    out.is_error = succeeded == 0;
    Ok(out)
}

// ---- definitions ---------------------------------------------------------------------------------

fn file_prop() -> Value {
    json!({
        "type": "string",
        "description": "Local file path (absolute is safest; relative paths resolve against the server's working \
                        directory) or a public http(s) URL. PDF, images and the office formats the provider accepts.",
    })
}

fn model_prop(mode: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "Model as \"<provider>/<model>\", e.g. \"reducto/standard\", or a bare provider name for its default \
             {mode} model. It must serve the {mode} mode; list_models with mode=\"{mode}\" lists the candidates."
        ),
    })
}

fn pages_prop() -> Value {
    json!({ "type": "string", "description": "1-based page selection, e.g. \"1-3,7\". Limits cost on long documents." })
}

fn language_prop() -> Value {
    json!({ "type": "string", "description": "Language hint (ISO 639-1), forwarded when the provider supports it." })
}

fn max_chars_prop() -> Value {
    json!({
        "type": "integer", "minimum": 1, "maximum": MAX_MAX_CHARS,
        "description": format!("Return at most this many characters of document text (default {DEFAULT_MAX_CHARS}); \
                                longer output is truncated with a note."),
    })
}

fn cost_schema() -> Value {
    json!({ "type": ["number", "null"], "description": "Estimated USD cost (provider-reported or list price); null when unpriced." })
}

fn run_props() -> Map<String, Value> {
    let v = json!({
        "model": { "type": "string" },
        "provider": { "type": "string" },
        "pages": { "type": "integer", "description": "Pages processed / billed." },
        "latency_ms": { "type": "integer" },
        "cost_usd": cost_schema(),
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn object_schema(mut props: Map<String, Value>, extra: Value, required: &[&str]) -> Value {
    if let Value::Object(e) = extra {
        props.extend(e);
    }
    json!({ "type": "object", "properties": props, "required": required })
}

fn tool(
    features: Features,
    name: &str,
    title: &str,
    description: String,
    input: Value,
    output: Value,
    annotations: Value,
) -> Value {
    let mut t = json!({ "name": name, "description": description, "inputSchema": input });
    if features.structured {
        t["title"] = json!(title);
        t["outputSchema"] = output;
    }
    if features.annotations {
        let mut ann = annotations;
        ann["title"] = json!(title);
        t["annotations"] = ann;
    }
    t
}

/// Tool definitions for `tools/list`, in a fixed order.
pub(crate) fn definitions(features: Features) -> Vec<Value> {
    // Every call that reaches a provider is read-only on the user's side, but talks to an
    // external service (openWorld) and is billed, so it is not marked idempotent.
    let remote =
        json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true });
    let local =
        json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });

    let parse = tool(
        features,
        "parse",
        "Parse a document to markdown",
        format!(
            "Parse a document into layout-aware markdown (headings, lists, tables) with the chosen model. Returns the \
             markdown (or plain text with output=\"text\"), pages, latency and estimated cost; include_blocks adds a \
             per-block summary (type, page, box). {SENT_TO_PROVIDER}"
        ),
        json!({
            "type": "object",
            "properties": {
                "file": file_prop(),
                "model": model_prop("parse"),
                "pages": pages_prop(),
                "output": { "type": "string", "enum": ["markdown", "text"], "default": "markdown",
                            "description": "Return markdown (default) or plain text." },
                "include_blocks": { "type": "boolean", "default": false,
                                    "description": format!("Also return up to {MAX_BLOCKS} typed blocks (type, page, normalized box, confidence, excerpt).") },
                "language": language_prop(),
                "max_chars": max_chars_prop(),
            },
            "required": ["file", "model"],
            "additionalProperties": false,
        }),
        object_schema(
            run_props(),
            json!({
                "output": { "type": "string", "enum": ["markdown", "text"] },
                "content": { "type": "string", "description": "The document text (truncated to max_chars)." },
                "total_chars": { "type": "integer" },
                "truncated": { "type": "boolean" },
                "blocks": { "type": "array", "items": { "type": "object" } },
                "blocks_total": { "type": "integer" },
            }),
            &["model", "provider", "pages", "latency_ms", "cost_usd", "output", "content", "total_chars", "truncated"],
        ),
        remote.clone(),
    );

    let ocr = tool(
        features,
        "ocr",
        "OCR a document to plain text",
        format!(
            "Read the plain text of a document (no layout semantics) with the chosen model. Models without a native \
             OCR endpoint derive the text from their parse output (derived_from_parse=true). {SENT_TO_PROVIDER}"
        ),
        json!({
            "type": "object",
            "properties": {
                "file": file_prop(),
                "model": model_prop("ocr"),
                "pages": pages_prop(),
                "language": language_prop(),
                "max_chars": max_chars_prop(),
            },
            "required": ["file", "model"],
            "additionalProperties": false,
        }),
        object_schema(
            run_props(),
            json!({
                "derived_from_parse": { "type": "boolean" },
                "content": { "type": "string", "description": "The document text (truncated to max_chars)." },
                "total_chars": { "type": "integer" },
                "truncated": { "type": "boolean" },
            }),
            &[
                "model",
                "provider",
                "pages",
                "latency_ms",
                "cost_usd",
                "derived_from_parse",
                "content",
                "total_chars",
                "truncated",
            ],
        ),
        remote.clone(),
    );

    let extract = tool(
        features,
        "extract",
        "Extract JSON from a document",
        format!(
            "Extract a JSON object shaped by a JSON Schema from a document (invoices, forms, contracts) with the chosen \
             extract-mode model. Returns the data plus per-field confidence and citations where the provider reports \
             them. {SENT_TO_PROVIDER}"
        ),
        json!({
            "type": "object",
            "properties": {
                "file": file_prop(),
                "model": model_prop("extract"),
                "schema": { "type": "object", "description": "JSON Schema (an object schema) describing the fields to extract." },
                "instructions": { "type": "string", "description": "Extra natural-language guidance for the extractor." },
                "citations": { "type": "boolean", "default": false,
                               "description": "Ask for per-field citations (page, box, source text) where supported." },
                "pages": pages_prop(),
                "language": language_prop(),
            },
            "required": ["file", "model", "schema"],
            "additionalProperties": false,
        }),
        object_schema(
            run_props(),
            json!({
                "data": { "description": "The extracted object, shaped by the schema." },
                "fields": { "type": "object", "description": "Per-field confidence and citations keyed by JSON pointer." },
            }),
            &["model", "provider", "pages", "latency_ms", "cost_usd", "data", "fields"],
        ),
        remote.clone(),
    );

    let list_models = tool(
        features,
        "list_models",
        "List models, modes and prices",
        "List the models this server can call: model string, modes (parse, ocr, extract), price per page by mode, \
         whether the provider's API key is set in the server's environment (the key itself is never shown), and \
         whether it is a local engine. Makes no network call and costs nothing."
            .to_string(),
        json!({
            "type": "object",
            "properties": {
                "mode": { "type": "string", "enum": ["parse", "ocr", "extract"], "description": "Only models that serve this mode." },
                "provider": { "type": "string", "description": "Only this provider, e.g. \"reducto\"." },
                "include_descriptions": { "type": "boolean", "default": false,
                                          "description": "Add a one-line description of each model (longer output)." },
            },
            "additionalProperties": false,
        }),
        json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
                "models": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "model": { "type": "string" },
                            "provider": { "type": "string" },
                            "modes": { "type": "array", "items": { "type": "string" } },
                            "default": { "type": "boolean" },
                            "per_page_usd": { "type": "object", "additionalProperties": { "type": "number" } },
                            "self_hosted": { "type": "boolean" },
                            "env_var": { "type": ["string", "null"] },
                            "key_required": { "type": "boolean" },
                            "key_configured": { "type": "boolean" },
                            "ready": { "type": "boolean" },
                            "description": { "type": "string" },
                        },
                        "required": ["model", "provider", "modes", "default", "per_page_usd", "key_configured", "ready"],
                    },
                },
                "allow_list": { "type": ["array", "null"], "items": { "type": "string" } },
                "note": { "type": "string" },
            },
            "required": ["count", "models"],
        }),
        local,
    );

    let compare = tool(
        features,
        "compare",
        "Compare models on one document",
        format!(
            "Run one document through several models concurrently (at most {MAX_COMPARE_MODELS}) in parse or ocr mode \
             and report, per model: success or the typed error, pages, latency, estimated cost, output length and an \
             excerpt of the output. Every model is billed separately. {SENT_TO_PROVIDER}"
        ),
        json!({
            "type": "object",
            "properties": {
                "file": file_prop(),
                "models": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": MAX_COMPARE_MODELS,
                            "description": "Model strings to compare, e.g. [\"reducto/standard\", \"llamaparse/cost_effective\"]." },
                "mode": { "type": "string", "enum": ["parse", "ocr"], "default": "parse" },
                "pages": pages_prop(),
                "language": language_prop(),
                "excerpt_chars": { "type": "integer", "minimum": 1, "maximum": MAX_EXCERPT_CHARS,
                                   "description": format!("Characters of output to show per model (default {DEFAULT_EXCERPT_CHARS}).") },
            },
            "required": ["file", "models"],
            "additionalProperties": false,
        }),
        json!({
            "type": "object",
            "properties": {
                "mode": { "type": "string" },
                "file": { "type": "string" },
                "succeeded": { "type": "integer" },
                "failed": { "type": "integer" },
                "total_cost_usd": cost_schema(),
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "model": { "type": "string" },
                            "ok": { "type": "boolean" },
                            "pages": { "type": ["integer", "null"] },
                            "latency_ms": { "type": "integer" },
                            "cost_usd": cost_schema(),
                            "chars": { "type": ["integer", "null"] },
                            "excerpt": { "type": ["string", "null"] },
                            "excerpt_truncated": { "type": "boolean" },
                            "error": {
                                "type": ["object", "null"],
                                "properties": {
                                    "kind": { "type": "string" },
                                    "message": { "type": "string" },
                                    "provider": { "type": ["string", "null"] },
                                    "status_code": { "type": ["integer", "null"] },
                                    "retryable": { "type": "boolean" },
                                },
                            },
                        },
                        "required": ["model", "ok", "latency_ms", "error"],
                    },
                },
            },
            "required": ["mode", "file", "succeeded", "failed", "total_cost_usd", "results"],
        }),
        remote,
    );

    vec![parse, ocr, extract, list_models, compare]
}
