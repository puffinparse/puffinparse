//! `puffinparse mcp`: a Model Context Protocol server over stdio.
//!
//! Hand-written on `serde_json` (no MCP SDK): newline-delimited JSON-RPC 2.0, one message per line.
//! stdout carries protocol messages only; diagnostics go to stderr through `tracing`.
//!
//! The server is *dual-era* (spec revision 2026-07-28, "Versioning: Backward Compatibility"):
//!
//! - **Modern** (`2026-07-28`): stateless. Every request carries
//!   `_meta["io.modelcontextprotocol/protocolVersion"]` and `.../clientCapabilities`; results carry
//!   `resultType: "complete"` and `_meta["io.modelcontextprotocol/serverInfo"]`; `server/discover`
//!   lists the supported versions. An unknown version gets `UnsupportedProtocolVersionError`
//!   (`-32022`).
//! - **Legacy** (`2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`): the `initialize` /
//!   `notifications/initialized` handshake, version negotiated once per process. This is what
//!   today's clients (Claude Code, Cursor, Codex) speak.
//!
//! Requests other than `tools/call` are answered inline; each `tools/call` runs on its own task so
//! a slow provider never blocks `tools/list` or a second call, and `notifications/cancelled` aborts
//! it. Every outgoing line is scrubbed of the provider keys found in the environment before it is
//! written, as a last line of defence: keys are never part of a tool result to begin with.

mod eval_tools;
mod tools;

#[cfg(test)]
mod tests;

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

pub use tools::{error_text, AllowList, Config};

/// Modern (stateless, per-request `_meta`) revisions this server implements.
pub const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// Handshake-based revisions, newest first. The first one is offered when a client asks for a
/// version we do not know.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

// JSON-RPC 2.0 and MCP error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "PuffinParse turns documents (PDF, images, office files) into markdown, plain text or \
schema-shaped JSON through one interface over many OCR providers. Call list_models first: it shows every \
model string (<provider>/<model>), the modes it serves (parse, ocr, extract), its price per page and whether \
its provider is ready (API key set in this server's environment, or a local engine). Each parse, ocr, \
extract or compare call uploads the document to the provider of the chosen model, which bills the user at \
its per-page price; local engines (tesseract, docling, paddleocr, vllm) keep the document on the user's \
machine or server. Use `pages` to limit cost on long documents, and prefer one model over compare unless \
the user wants a comparison. Files are local paths (absolute paths are safest) or public http(s) URLs. \
When the user asks to find the best model for their own documents (\"use puffinparse to find the best model for \
the files in ~/docs\"), do it with one question at most: scan_dataset; recommend_models with only what the user \
already said (no questionnaire, the defaults are fine); pick up to four shortlisted models whose key is ready plus \
a free local engine; run_dataset with dry_run; then ask once: run these models on these documents for about $X, \
capped at $Y? Start it with max_cost_usd after a yes. Then run_status, analyze_run, read the queued documents and \
record_review, and finish with the recommendation and the open_viewer link. The find_best_model prompt has the \
details.";

/// Run the server on this process's stdin / stdout until stdin closes.
pub async fn run_stdio(cfg: Config) -> anyhow::Result<()> {
    tracing::info!(allow = %cfg.allow.describe(), "puffinparse mcp: serving on stdio");
    serve(cfg, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}

/// Serve MCP over any byte stream pair (stdio in production, in-memory pipes in tests).
///
/// Returns when `reader` reaches end-of-file; in-flight tool calls are then aborted (the client
/// closed the connection, so nobody is left to read their results).
pub async fn serve<R, W>(cfg: Config, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let server = Arc::new(Server::new(cfg));
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let secrets = secrets_from_env();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = rx.recv().await {
            // serde_json escapes control characters, so a serialized message never spans lines.
            let mut line = serde_json::to_string(&msg).unwrap_or_else(|e| {
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32603, "message": format!("serialization failed: {e}")}})
                    .to_string()
            });
            redact(&mut line, &secrets);
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).await? == 0 {
            break;
        }
        let line = buf.trim_ascii();
        if !line.is_empty() {
            server.handle_line(line, &tx);
        }
    }
    server.abort_all();
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// A JSON-RPC error object.
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    fn to_json(&self) -> Value {
        let mut e = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.data {
            e["data"] = d.clone();
        }
        e
    }
}

/// Which protocol generation a request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    Modern,
    Legacy(&'static str),
}

/// Optional tool-schema features, gated by the protocol revision in use.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Features {
    /// `title`, `outputSchema` and `structuredContent` (2025-06-18+).
    pub structured: bool,
    /// Tool `annotations` (2025-03-26+).
    pub annotations: bool,
}

impl Era {
    fn features(self) -> Features {
        match self {
            Era::Modern => Features { structured: true, annotations: true },
            // Revision strings are ISO dates, so they compare correctly as strings.
            Era::Legacy(v) => Features { structured: v >= "2025-06-18", annotations: v >= "2025-03-26" },
        }
    }
}

struct Server {
    cfg: Arc<Config>,
    /// Version agreed by `initialize` (legacy clients); `None` until then.
    negotiated: Mutex<Option<&'static str>>,
    /// In-flight `tools/call` tasks by serialized request id, for `notifications/cancelled`.
    inflight: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

impl Server {
    fn new(cfg: Config) -> Self {
        Self { cfg: Arc::new(cfg), negotiated: Mutex::new(None), inflight: Mutex::new(HashMap::new()) }
    }

    fn abort_all(&self) {
        for (_, h) in self.inflight.lock().unwrap_or_else(|p| p.into_inner()).drain() {
            h.abort();
        }
    }

    fn handle_line(self: &Arc<Self>, line: &[u8], tx: &mpsc::UnboundedSender<Value>) {
        let send = |v: Value| {
            let _ = tx.send(v);
        };
        let msg: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(e) => {
                return send(error_response(Value::Null, &RpcError::new(PARSE_ERROR, format!("Parse error: {e}"))))
            }
        };
        let Value::Object(obj) = msg else {
            let why =
                if msg.is_array() { "JSON-RPC batches are not supported by MCP" } else { "expected a JSON object" };
            return send(error_response(
                Value::Null,
                &RpcError::new(INVALID_REQUEST, format!("Invalid Request: {why}")),
            ));
        };
        let id = obj.get("id").cloned();
        let valid_id = id.as_ref().filter(|i| i.is_string() || i.is_i64() || i.is_u64()).cloned();
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let err = RpcError::new(INVALID_REQUEST, "Invalid Request: \"jsonrpc\" must be \"2.0\"");
            return send(error_response(valid_id.unwrap_or(Value::Null), &err));
        }
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            if obj.contains_key("result") || obj.contains_key("error") {
                return; // A response; this server never sends requests, so there is nothing to match.
            }
            let err = RpcError::new(INVALID_REQUEST, "Invalid Request: missing \"method\"");
            return send(error_response(valid_id.unwrap_or(Value::Null), &err));
        };
        let params = match obj.get("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => {
                // A malformed notification gets no reply.
                if let Some(id) = valid_id {
                    send(error_response(id, &RpcError::new(INVALID_PARAMS, "\"params\" must be an object")));
                }
                return;
            }
        };
        match (id, valid_id) {
            (None, _) => self.notification(method, &params),
            (Some(_), Some(id)) => self.request(id, method, params, tx),
            (Some(_), None) => send(error_response(
                Value::Null,
                &RpcError::new(INVALID_REQUEST, "Invalid Request: \"id\" must be a string or an integer"),
            )),
        }
    }

    fn notification(&self, method: &str, params: &Map<String, Value>) {
        match method {
            "notifications/cancelled" => {
                if let Some(rid) = params.get("requestId") {
                    let key = rid.to_string();
                    if let Some(h) = self.inflight.lock().unwrap_or_else(|p| p.into_inner()).remove(&key) {
                        tracing::info!(request = %key, "puffinparse mcp: cancelled");
                        h.abort();
                    }
                }
            }
            // `notifications/initialized` and anything else: nothing to do.
            other => tracing::debug!(method = other, "puffinparse mcp: notification"),
        }
    }

    fn request(
        self: &Arc<Self>,
        id: Value,
        method: &str,
        params: Map<String, Value>,
        tx: &mpsc::UnboundedSender<Value>,
    ) {
        let era = match self.era(&params) {
            Ok(e) => e,
            Err(err) => {
                let _ = tx.send(error_response(id, &err));
                return;
            }
        };
        if method != "tools/call" {
            let result = self.dispatch(method, &params, era);
            let _ = tx.send(respond(id, result, era));
            return;
        }
        // Protocol-level checks happen here; anything past them is a tool result (isError).
        let (name, args) = match tool_call_params(&params) {
            Ok(v) => v,
            Err(err) => {
                let _ = tx.send(error_response(id, &err));
                return;
            }
        };
        let key = id.to_string();
        let server = self.clone();
        let tx = tx.clone();
        let task_key = key.clone();
        let mut inflight = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        // JSON-RPC ids must be unique among in-flight requests. Accepting a duplicate would replace
        // the first call's abort handle, leaving it uncancellable while it keeps billing.
        if inflight.contains_key(&key) {
            let err = RpcError::new(INVALID_REQUEST, format!("Invalid Request: request id {key} is already in flight"));
            let _ = tx.send(error_response(id, &err));
            return;
        }
        let handle = tokio::spawn(async move {
            tracing::info!(tool = %name, "puffinparse mcp: tools/call");
            let out = tools::call(&server.cfg, &name, &args).await;
            server.inflight.lock().unwrap_or_else(|p| p.into_inner()).remove(&task_key);
            let _ = tx.send(respond(id, Ok(out.to_json(era.features())), era));
        });
        inflight.insert(key, handle.abort_handle());
    }

    /// Resolve the protocol generation of one request from its `_meta`.
    fn era(&self, params: &Map<String, Value>) -> Result<Era, RpcError> {
        let meta = params.get("_meta").and_then(Value::as_object);
        let Some(version) = meta.and_then(|m| m.get(META_VERSION)) else {
            let negotiated = *self.negotiated.lock().unwrap_or_else(|p| p.into_inner());
            return Ok(Era::Legacy(negotiated.unwrap_or(LEGACY_VERSIONS[0])));
        };
        let Some(version) = version.as_str() else {
            return Err(RpcError::new(INVALID_PARAMS, format!("_meta[\"{META_VERSION}\"] must be a string")));
        };
        if MODERN_VERSIONS.contains(&version) {
            if !meta.and_then(|m| m.get(META_CLIENT_CAPS)).is_some_and(Value::is_object) {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    format!("missing required _meta field \"{META_CLIENT_CAPS}\""),
                ));
            }
            return Ok(Era::Modern);
        }
        if let Some(v) = LEGACY_VERSIONS.iter().find(|v| **v == version) {
            return Ok(Era::Legacy(v));
        }
        Err(RpcError {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".into(),
            data: Some(json!({ "supported": supported_versions(), "requested": version })),
        })
    }

    fn dispatch(&self, method: &str, params: &Map<String, Value>, era: Era) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::new(INVALID_PARAMS, "initialize: \"protocolVersion\" is required"))?;
                // Echo a version we support; otherwise offer our newest handshake revision.
                let version = LEGACY_VERSIONS.iter().copied().find(|v| *v == requested).unwrap_or(LEGACY_VERSIONS[0]);
                *self.negotiated.lock().unwrap_or_else(|p| p.into_inner()) = Some(version);
                tracing::info!(requested, version, "puffinparse mcp: initialize");
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false }, "prompts": { "listChanged": false } },
                    "serverInfo": server_info(),
                    "instructions": INSTRUCTIONS,
                }))
            }
            "server/discover" => Ok(json!({
                "resultType": "complete",
                "supportedVersions": supported_versions(),
                "capabilities": { "tools": {}, "prompts": {} },
                "_meta": { META_SERVER_INFO: server_info() },
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let mut defs = tools::definitions(era.features());
                defs.extend(eval_tools::definitions(era.features()));
                let mut result = json!({ "tools": defs });
                if era == Era::Modern {
                    // CacheableResult (2026-07-28): the list is fixed for the life of the process.
                    result["ttlMs"] = json!(3_600_000);
                    result["cacheScope"] = json!("public");
                }
                Ok(result)
            }
            "prompts/list" => Ok(json!({ "prompts": eval_tools::prompt_definitions() })),
            "prompts/get" => eval_tools::get_prompt(params).map_err(|e| RpcError::new(INVALID_PARAMS, e)),
            other => Err(RpcError::new(METHOD_NOT_FOUND, format!("Method not found: {other}"))),
        }
    }
}

/// Validate `tools/call` params: a known tool name and an object of arguments.
fn tool_call_params(params: &Map<String, Value>) -> Result<(String, Map<String, Value>), RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "tools/call: \"name\" (string) is required"))?;
    if !tools::NAMES.contains(&name) && !eval_tools::NAMES.contains(&name) {
        return Err(RpcError::new(INVALID_PARAMS, format!("Unknown tool: {name}")));
    }
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(RpcError::new(INVALID_PARAMS, "tools/call: \"arguments\" must be an object")),
    };
    Ok((name.to_string(), args))
}

fn supported_versions() -> Vec<&'static str> {
    MODERN_VERSIONS.iter().chain(LEGACY_VERSIONS).copied().collect()
}

fn server_info() -> Value {
    json!({
        "name": "puffinparse",
        "title": "PuffinParse",
        "version": puffinparse_core::VERSION,
        "description": "Parse, OCR and extract documents with any OCR provider, using your own provider keys.",
        "websiteUrl": "https://puffinparse.com",
    })
}

fn respond(id: Value, result: Result<Value, RpcError>, era: Era) -> Value {
    match result {
        Ok(mut result) => {
            if era == Era::Modern {
                if let Value::Object(obj) = &mut result {
                    obj.entry("resultType").or_insert(json!("complete"));
                    let meta = obj.entry("_meta").or_insert_with(|| json!({}));
                    if let Value::Object(meta) = meta {
                        meta.entry(META_SERVER_INFO).or_insert_with(server_info);
                    }
                }
            }
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        }
        Err(e) => error_response(id, &e),
    }
}

fn error_response(id: Value, err: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": err.to_json() })
}

/// Environment variables that may hold credentials. Their values never appear in a tool result
/// (results are built from provider output, not from configuration), but every outgoing line is
/// scrubbed of them anyway in case a provider echoes a key back in an error message.
const EXTRA_SECRET_VARS: &[&str] = &[
    "PUFFINPARSE_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "MATHPIX_APP_ID",
    "DOCLING_API_KEY",
    "VLLM_API_KEY",
];

fn secrets_from_env() -> Vec<String> {
    let mut names: Vec<&str> =
        puffinparse_core::PROVIDERS.iter().map(|p| p.env_var).filter(|v| !v.is_empty()).collect();
    names.extend(EXTRA_SECRET_VARS);
    let mut secrets: Vec<String> = names
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .map(|v| v.trim().to_string())
        // Short values (a region, "1") would only cause false positives.
        .filter(|v| v.len() >= 8)
        .collect();
    secrets.sort();
    secrets.dedup();
    secrets
}

/// Replace every secret in an outgoing (already serialised) line. A secret containing a quote,
/// backslash or control character appears JSON-escaped in the line, so that form is matched too.
fn redact(line: &mut String, secrets: &[String]) {
    for s in secrets {
        let escaped = serde_json::to_string(s).map(|q| q[1..q.len() - 1].to_string()).unwrap_or_default();
        for form in [s.as_str(), escaped.as_str()] {
            if !form.is_empty() && line.contains(form) {
                *line = line.replace(form, "[redacted]");
            }
        }
    }
}
