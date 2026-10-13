//! Protocol-level tests: the server is driven over in-memory pipes exactly as a client would drive
//! it over stdio, and the provider is a loopback mock of Mistral's `POST /v1/ocr` answering with
//! the core's recorded (redacted) fixtures. No network.

use super::{serve, AllowList, Config};
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};

const OCR_FIXTURE: &str = include_str!("../../../puffinparse-core/tests/fixtures/mistral_ocr.json");
const ANNOTATION_FIXTURE: &str = include_str!("../../../puffinparse-core/tests/fixtures/mistral_annotation.json");
/// The key handed to the mock through the test-only endpoint override. It must never be echoed.
const TEST_KEY: &str = "sk-test-mistral-0123456789";

/// Start the mock provider; returns `http://127.0.0.1:<port>`.
async fn mock() -> String {
    let json = |body: &'static str| ([(axum::http::header::CONTENT_TYPE, "application/json")], body);
    let app = axum::Router::new()
        .route("/ok/v1/ocr", post(move || async move { json(OCR_FIXTURE) }))
        .route("/ann/v1/ocr", post(move || async move { json(ANNOTATION_FIXTURE) }))
        .route(
            "/unauth/v1/ocr",
            post(|| async { (StatusCode::UNAUTHORIZED, axum::Json(json!({ "message": "Invalid API key" }))) }),
        )
        .route(
            "/fail/v1/ocr",
            post(|| async {
                (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "message": "mock upstream exploded" })))
            }),
        )
        .route(
            "/slow/v1/ocr",
            post(move || async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                json(OCR_FIXTURE)
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Config routing mistral models to the mock: `ocr-latest` → ok, `ocr-4-1` → annotation,
/// `ocr-2512` → 500, `ocr-4-0` → 401; the provider-level entry points at `/slow` (30 s).
async fn mock_config() -> Config {
    let base = mock().await;
    let mut cfg = Config { max_retries: 0, timeout_secs: 60.0, ..Config::default() };
    for (model, route) in [
        ("mistral/ocr-latest", "ok"),
        ("mistral/ocr-4-1", "ann"),
        ("mistral/ocr-2512", "fail"),
        ("mistral/ocr-4-0", "unauth"),
        ("mistral", "slow"),
    ] {
        cfg.endpoints.insert(model.into(), (format!("{base}/{route}"), TEST_KEY.into()));
    }
    cfg
}

struct Client {
    writer: WriteHalf<tokio::io::DuplexStream>,
    lines: Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    /// Every line the server wrote, for whole-session assertions.
    transcript: Vec<String>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Client {
    fn start(cfg: Config) -> Self {
        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server_end);
        let server = tokio::spawn(serve(cfg, sr, sw));
        let (cr, cw) = tokio::io::split(client_end);
        Self { writer: cw, lines: BufReader::new(cr).lines(), transcript: Vec::new(), server }
    }

    async fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, msg: Value) {
        self.send_raw(&msg.to_string()).await;
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
            .await
            .expect("timed out waiting for the server")
            .unwrap()
            .expect("server closed stdout");
        assert!(!line.contains('\n'));
        self.transcript.push(line.clone());
        let v: Value = serde_json::from_str(&line).expect("server wrote a non-JSON line");
        assert_eq!(v["jsonrpc"], "2.0", "{v}");
        v
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
        let resp = self.recv().await;
        assert_eq!(resp["id"], id, "{resp}");
        resp
    }

    async fn initialize(&mut self, version: &str) -> Value {
        let resp = self
            .request(
                0,
                "initialize",
                json!({ "protocolVersion": version, "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
            )
            .await;
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        resp
    }

    /// `tools/call`, returning the `result` object.
    async fn call(&mut self, id: i64, name: &str, args: Value) -> Value {
        let resp = self.request(id, "tools/call", json!({ "name": name, "arguments": args })).await;
        assert!(resp.get("error").is_none(), "unexpected protocol error: {resp}");
        resp["result"].clone()
    }
}

fn text(result: &Value) -> String {
    result["content"].as_array().unwrap().iter().map(|c| c["text"].as_str().unwrap()).collect::<Vec<_>>().join("\n")
}

/// A small file the mock never reads (Mistral inlines small images as a data URL).
fn temp_png(name: &str) -> String {
    let path = std::env::temp_dir().join(format!("puffinparse-mcp-{}-{name}.png", std::process::id()));
    std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot really a png").unwrap();
    path.to_string_lossy().into_owned()
}

fn modern_meta() -> Value {
    json!({ "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
    }})
}

// ---- lifecycle and listing -----------------------------------------------------------------------

#[tokio::test]
async fn initialize_then_list_tools() {
    let mut c = Client::start(Config::default());
    let init = c.initialize("2025-06-18").await;
    let r = &init["result"];
    assert_eq!(r["protocolVersion"], "2025-06-18");
    assert_eq!(r["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(r["serverInfo"]["name"], "puffinparse");
    assert_eq!(r["serverInfo"]["version"], puffinparse_core::VERSION);
    assert!(r["instructions"].as_str().unwrap().contains("list_models"));
    assert!(r.get("resultType").is_none(), "legacy results carry no resultType");

    // The initialized notification got no reply: the next line answers the next request.
    let list = c.request(1, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "parse",
            "ocr",
            "extract",
            "list_models",
            "compare",
            "scan_dataset",
            "recommend_models",
            "run_dataset",
            "run_status",
            "cancel_run",
            "analyze_run",
            "get_output",
            "record_review",
            "open_viewer",
            "anonymize_dataset"
        ]
    );
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["outputSchema"]["type"], "object", "{t}");
        assert!(t["title"].is_string());
        assert_eq!(t["annotations"]["destructiveHint"], false);
    }
    for t in &tools[..5] {
        assert_eq!(t["annotations"]["readOnlyHint"], true);
    }
    // run_dataset bills the user; the tools that write into a run folder are not read-only.
    let run = tools.iter().find(|t| t["name"] == "run_dataset").unwrap();
    assert_eq!(run["annotations"]["openWorldHint"], true);
    assert_eq!(run["annotations"]["readOnlyHint"], false);
    for name in ["parse", "ocr", "extract", "compare"] {
        let t = tools.iter().find(|t| t["name"] == name).unwrap();
        assert!(t["description"].as_str().unwrap().contains("sent to the provider"), "{name}");
        assert_eq!(t["annotations"]["openWorldHint"], true);
    }
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["file", "model"]));
    assert_eq!(tools[2]["inputSchema"]["required"], json!(["file", "model", "schema"]));

    let pong = c.request(2, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
}

#[tokio::test]
async fn version_negotiation_and_old_revisions() {
    // Unknown version: the server offers its newest handshake revision.
    let mut c = Client::start(Config::default());
    assert_eq!(c.initialize("1999-01-01").await["result"]["protocolVersion"], "2025-11-25");

    // 2024-11-05 predates titles, annotations, outputSchema and structuredContent.
    let mut c = Client::start(Config::default());
    assert_eq!(c.initialize("2024-11-05").await["result"]["protocolVersion"], "2024-11-05");
    let list = c.request(1, "tools/list", json!({})).await;
    for t in list["result"]["tools"].as_array().unwrap() {
        assert!(t.get("outputSchema").is_none() && t.get("annotations").is_none() && t.get("title").is_none());
    }
    let r = c.call(2, "list_models", json!({ "provider": "reducto" })).await;
    assert!(r.get("structuredContent").is_none());
    assert!(text(&r).contains("reducto/standard"));
}

#[tokio::test]
async fn modern_stateless_requests() {
    let mut c = Client::start(Config::default());
    let d = c.request(1, "server/discover", modern_meta()).await;
    let r = &d["result"];
    assert_eq!(r["resultType"], "complete");
    assert_eq!(r["supportedVersions"][0], "2026-07-28");
    assert!(r["supportedVersions"].as_array().unwrap().contains(&json!("2025-11-25")));
    assert!(r["capabilities"]["tools"].is_object());
    assert_eq!(r["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "puffinparse");

    // No initialize needed: every request carries its version.
    let list = c.request(2, "tools/list", modern_meta()).await;
    assert_eq!(list["result"]["resultType"], "complete");
    assert_eq!(list["result"]["cacheScope"], "public");
    assert!(list["result"]["ttlMs"].is_u64());
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 15);

    let mut params = modern_meta();
    params["name"] = json!("list_models");
    params["arguments"] = json!({ "mode": "extract" });
    let call = c.request(3, "tools/call", params).await;
    assert_eq!(call["result"]["resultType"], "complete");
    assert_eq!(call["result"]["isError"], false);
    assert!(call["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());

    // Unsupported version → -32022 with the supported list.
    let bad = c
        .request(
            4,
            "tools/list",
            json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "1900-01-01", "io.modelcontextprotocol/clientCapabilities": {} } }),
        )
        .await;
    assert_eq!(bad["error"]["code"], -32022);
    assert_eq!(bad["error"]["data"]["requested"], "1900-01-01");
    assert!(bad["error"]["data"]["supported"].as_array().unwrap().contains(&json!("2026-07-28")));

    // A modern request without clientCapabilities is malformed.
    let missing = c
        .request(5, "tools/list", json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } }))
        .await;
    assert_eq!(missing["error"]["code"], -32602);
}

// ---- malformed input -----------------------------------------------------------------------------

#[tokio::test]
async fn malformed_messages_get_jsonrpc_errors() {
    let mut c = Client::start(Config::default());
    c.initialize("2025-11-25").await;

    c.send_raw("{not json").await;
    let e = c.recv().await;
    assert_eq!(e["error"]["code"], -32700);
    assert_eq!(e["id"], Value::Null);

    c.send_raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
    assert_eq!(c.recv().await["error"]["code"], -32600);

    c.send_raw(r#"{"id":7,"method":"ping"}"#).await;
    let e = c.recv().await;
    assert_eq!((e["id"].clone(), e["error"]["code"].clone()), (json!(7), json!(-32600)));

    c.send_raw(r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#).await;
    assert_eq!(c.recv().await["error"]["code"], -32600);

    c.send_raw(r#"{"jsonrpc":"2.0","id":8,"method":"ping","params":[1]}"#).await;
    assert_eq!(c.recv().await["error"]["code"], -32602);

    // Blank lines and stray responses are ignored; unknown notifications get no reply.
    c.send_raw("").await;
    c.send_raw(r#"{"jsonrpc":"2.0","id":99,"result":{}}"#).await;
    c.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#).await;

    assert_eq!(c.request(9, "resources/list", json!({})).await["error"]["code"], -32601);
    assert_eq!(c.request(10, "initialize", json!({})).await["error"]["code"], -32602);

    let unknown = c.request(11, "tools/call", json!({ "name": "delete_everything", "arguments": {} })).await;
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(unknown["error"]["message"].as_str().unwrap().contains("Unknown tool: delete_everything"));

    let no_name = c.request(12, "tools/call", json!({ "arguments": {} })).await;
    assert_eq!(no_name["error"]["code"], -32602);
    let bad_args = c.request(13, "tools/call", json!({ "name": "parse", "arguments": "file.pdf" })).await;
    assert_eq!(bad_args["error"]["code"], -32602);

    // Argument problems are tool errors the model can fix, not protocol errors.
    let r = c.call(14, "parse", json!({ "file": "a.pdf" })).await;
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("'model' is required"));
    let r = c.call(15, "parse", json!({ "file": "a.pdf", "model": "mistral", "page": "1" })).await;
    assert!(text(&r).contains("unknown argument 'page'"), "{r}");
    let r = c.call(16, "parse", json!({ "file": "ftp://host/a.pdf", "model": "mistral" })).await;
    assert!(text(&r).contains("unsupported URL scheme 'ftp'"), "{r}");
    let r = c.call(17, "parse", json!({ "file": "a.pdf", "model": "reducto/extract" })).await;
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("unsupported_model_error") && text(&r).contains("list_models"), "{r}");
    let r = c.call(18, "parse", json!({ "file": "a.pdf", "model": "nope/nope" })).await;
    assert!(text(&r).contains("unknown provider 'nope'"), "{r}");
    let r = c.call(19, "list_models", json!({ "mode": "summarize" })).await;
    assert!(text(&r).contains("unknown mode"), "{r}");

    // EOF ends the server cleanly.
    c.writer.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), c.server).await.expect("server did not exit on EOF").unwrap().unwrap();
}

// ---- tools against the mock provider -------------------------------------------------------------

#[tokio::test]
async fn parse_ocr_and_extract_through_the_mock() {
    let mut c = Client::start(mock_config().await);
    c.initialize("2025-11-25").await;
    let file = temp_png("tools");

    let r = c.call(1, "parse", json!({ "file": file, "model": "mistral", "include_blocks": true })).await;
    assert_eq!(r["isError"], false, "{r}");
    let md = r["content"][0]["text"].as_str().unwrap();
    assert!(md.contains("# Hello LiteOCR") && md.contains("Invoice #1234"), "{md}");
    let s = &r["structuredContent"];
    assert_eq!(s["model"], "mistral/ocr-latest");
    assert_eq!(s["pages"], 2);
    assert_eq!(s["output"], "markdown");
    assert_eq!(s["truncated"], false);
    assert!(s["cost_usd"].as_f64().unwrap() > 0.0);
    assert!(s["blocks"].as_array().is_some() && s["blocks_total"].is_u64());
    // The body is in structuredContent too (Claude Code shows only that); the second text block is
    // the metadata without the body, so a text-only client sees the body once.
    assert_eq!(s["content"].as_str().unwrap(), md, "structuredContent carries the same body");
    let meta: Value = serde_json::from_str(r["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(meta["model"], "mistral/ocr-latest");
    assert!(meta.get("content").is_none());
    let mut without_body = s.clone();
    without_body.as_object_mut().unwrap().remove("content");
    assert_eq!(meta, without_body, "the second text block is structuredContent minus the body");

    // Truncation, with a note telling the model how to get the rest.
    let r = c
        .call(2, "parse", json!({ "file": format!("file://{file}"), "model": "mistral/ocr-latest", "max_chars": 10 }))
        .await;
    let s = &r["structuredContent"];
    assert_eq!(s["truncated"], true);
    assert!(r["content"][0]["text"].as_str().unwrap().starts_with("# Hello Li\n\n[truncated by puffinparse"), "{r}");
    assert!(s["content"].as_str().unwrap().starts_with("# Hello Li\n\n[truncated by puffinparse"), "{r}");
    assert!(text(&r).contains("[truncated by puffinparse: showing the first 10 of"), "{r}");

    let r = c.call(3, "ocr", json!({ "file": file, "model": "mistral/ocr-latest" })).await;
    assert_eq!(r["isError"], false, "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("Invoice #1234"));
    assert!(r["structuredContent"]["content"].as_str().unwrap().contains("Invoice #1234"), "{r}");
    assert!(!r["content"][0]["text"].as_str().unwrap().contains("| Item |"), "ocr returns plain text");

    let schema = json!({ "type": "object", "properties": { "invoice_number": { "type": "string" }, "total": { "type": "number" } } });
    let r = c.call(4, "extract", json!({ "file": file, "model": "mistral/ocr-4-1", "schema": schema })).await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["data"]["invoice_number"], "INV-1234");
    assert_eq!(r["structuredContent"]["data"]["total"], 56.78);
    let data: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["total"], 56.78);
    // A JSON-encoded schema string is accepted too.
    let r =
        c.call(5, "extract", json!({ "file": file, "model": "mistral/ocr-4-1", "schema": schema.to_string() })).await;
    assert_eq!(r["isError"], false, "{r}");
    let r = c.call(6, "extract", json!({ "file": file, "model": "mistral/ocr-4-1", "schema": [1] })).await;
    assert!(text(&r).contains("JSON Schema object"));

    assert!(c.transcript.iter().all(|l| !l.contains(TEST_KEY)), "the provider key leaked into a response");
}

#[tokio::test]
async fn provider_failures_are_tool_errors() {
    let mut c = Client::start(mock_config().await);
    c.initialize("2025-11-25").await;
    let file = temp_png("errors");

    let r = c.call(1, "parse", json!({ "file": file, "model": "mistral/ocr-4-0" })).await;
    assert_eq!(r["isError"], true);
    assert!(r.get("structuredContent").is_none());
    let t = text(&r);
    assert!(t.contains("authentication_error [mistral]: Invalid API key (HTTP 401)"), "{t}");
    assert!(t.contains("Set MISTRAL_API_KEY in the environment"), "{t}");

    let r = c.call(2, "ocr", json!({ "file": file, "model": "mistral/ocr-2512" })).await;
    assert!(text(&r).contains("provider_error [mistral]: mock upstream exploded (HTTP 500)"), "{r}");

    // Input validation is the core's: a missing file fails before any network call.
    let r = c.call(3, "parse", json!({ "file": "/definitely/not/here.pdf", "model": "mistral/ocr-latest" })).await;
    assert!(text(&r).contains("input_error") && text(&r).contains("cannot read /definitely/not/here.pdf"), "{r}");

    assert!(c.transcript.iter().all(|l| !l.contains(TEST_KEY)));
}

#[tokio::test]
async fn compare_runs_models_concurrently_and_reports_each() {
    let mut c = Client::start(mock_config().await);
    c.initialize("2025-11-25").await;
    let file = temp_png("compare");

    let r = c
        .call(
            1,
            "compare",
            json!({ "file": file, "models": ["mistral/ocr-latest", "mistral/ocr-2512", "mistral/ocr-latest"], "excerpt_chars": 20 }),
        )
        .await;
    assert_eq!(r["isError"], false, "one success is enough: {r}");
    let s = &r["structuredContent"];
    assert_eq!(s["mode"], "parse");
    assert_eq!((s["succeeded"].clone(), s["failed"].clone()), (json!(1), json!(1)), "duplicates are dropped");
    let ok = &s["results"][0];
    assert_eq!(ok["model"], "mistral/ocr-latest");
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["excerpt"].as_str().unwrap().chars().count(), 20);
    assert_eq!(ok["excerpt_truncated"], true);
    assert!(ok["chars"].as_u64().unwrap() > 20);
    let bad = &s["results"][1];
    assert_eq!(bad["ok"], false);
    assert_eq!(bad["error"]["kind"], "provider");
    assert_eq!(bad["error"]["status_code"], 500);
    assert_eq!(s["total_cost_usd"], ok["cost_usd"]);
    let t = text(&r);
    assert!(
        t.contains("| mistral/ocr-latest | ok | 2 |") && t.contains("| mistral/ocr-2512 | error: provider_error"),
        "{t}"
    );

    // Every model failing makes the whole call an error, still with per-model detail.
    let r = c
        .call(2, "compare", json!({ "file": file, "models": ["mistral/ocr-2512", "mistral/ocr-4-0"], "mode": "ocr" }))
        .await;
    assert_eq!(r["isError"], true);
    assert_eq!(r["structuredContent"]["failed"], 2);

    // Validation happens for every model before anything runs.
    let r = c.call(3, "compare", json!({ "file": file, "models": ["mistral", "reducto/extract"] })).await;
    assert!(text(&r).contains("does not support mode 'parse'"), "{r}");
    let r = c.call(4, "compare", json!({ "file": file, "models": [] })).await;
    assert!(text(&r).contains("at least one model"));
    let r = c.call(5, "compare", json!({ "file": file, "models": ["mistral"], "mode": "extract" })).await;
    assert!(text(&r).contains("calling extract per model"));
}

// ---- allow-list, key safety, cancellation --------------------------------------------------------

#[tokio::test]
async fn allow_list_limits_calls_and_listing() {
    let mut cfg = mock_config().await;
    cfg.allow = AllowList::new(&["mistral/ocr-latest".into(), "llama".into()]).unwrap();
    let mut c = Client::start(cfg);
    c.initialize("2025-11-25").await;

    let r = c.call(1, "parse", json!({ "file": temp_png("allow"), "model": "reducto/standard" })).await;
    assert_eq!(r["isError"], true);
    assert!(text(&r).contains("'reducto/standard' is not enabled on this server"), "{r}");

    let r = c.call(2, "list_models", json!({})).await;
    let models: Vec<String> = r["structuredContent"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap().to_string())
        .collect();
    assert!(models.contains(&"mistral/ocr-latest".to_string()));
    assert!(models.iter().any(|m| m.starts_with("llamaparse/")));
    assert!(models.iter().all(|m| m == "mistral/ocr-latest" || m.starts_with("llamaparse/")), "{models:?}");
    assert_eq!(r["structuredContent"]["allow_list"], json!(["mistral/ocr-latest", "llamaparse/*"]));

    assert!(AllowList::new(&["reducto/nope".into()]).is_err());
    assert!(AllowList::new(&["nope/*".into()]).is_err());
    assert!(AllowList::new(&["*".into()]).unwrap().allows("reducto/standard"));
}

#[tokio::test]
async fn list_models_reports_keys_without_echoing_them() {
    // Only this test touches UPSTAGE_API_KEY.
    let secret = "up-secret-value-should-never-appear";
    std::env::set_var("UPSTAGE_API_KEY", secret);
    let mut c = Client::start(Config::default());
    c.initialize("2025-11-25").await;
    let r = c.call(1, "list_models", json!({ "provider": "upstage" })).await;
    let s = &r["structuredContent"];
    assert!(s["count"].as_u64().unwrap() > 0);
    for m in s["models"].as_array().unwrap() {
        assert_eq!(m["provider"], "upstage");
        assert_eq!(m["env_var"], "UPSTAGE_API_KEY");
        assert_eq!(m["key_configured"], true);
        assert_eq!(m["ready"], true);
    }
    let r = c.call(2, "list_models", json!({ "provider": "tesseract", "mode": "ocr" })).await;
    let m = &r["structuredContent"]["models"][0];
    assert_eq!(
        (m["self_hosted"].clone(), m["ready"].clone(), m["key_required"].clone()),
        (json!(true), json!(true), json!(false))
    );
    assert!(m["per_page_usd"].is_object());
    assert!(m.get("description").is_none(), "descriptions are opt-in");
    let r = c.call(4, "list_models", json!({ "provider": "tesseract", "include_descriptions": true })).await;
    assert!(r["structuredContent"]["models"][0]["description"].is_string());
    assert!(m["modes"].as_array().unwrap().contains(&json!("ocr")));

    let r = c.call(3, "list_models", json!({ "mode": "extract" })).await;
    for m in r["structuredContent"]["models"].as_array().unwrap() {
        assert!(m["modes"].as_array().unwrap().contains(&json!("extract")), "{m}");
    }
    assert!(c.transcript.iter().all(|l| !l.contains(secret)));
    std::env::remove_var("UPSTAGE_API_KEY");

    // The writer scrubs configured keys even if something did echo one.
    let mut line = format!("{{\"text\":\"key is {secret}\"}}");
    super::redact(&mut line, &[secret.to_string()]);
    assert_eq!(line, "{\"text\":\"key is [redacted]\"}");
    // A key with characters JSON escapes is matched in its escaped form as well.
    let odd = "k3y\"with\\quote\tand-tab";
    let mut line = serde_json::to_string(&json!({ "text": format!("echo: {odd}") })).unwrap();
    assert!(!line.contains(odd), "the raw form is not in the serialised line");
    super::redact(&mut line, &[odd.to_string()]);
    assert_eq!(line, "{\"text\":\"echo: [redacted]\"}");
}

#[tokio::test]
async fn cancelled_calls_send_no_result_and_do_not_block_others() {
    let mut cfg = mock_config().await;
    let slow = cfg.endpoints["mistral"].clone();
    cfg.endpoints.insert("mistral/ocr-latest".into(), slow);
    let mut c = Client::start(cfg);
    c.initialize("2025-11-25").await;
    let file = temp_png("cancel");

    c.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": { "name": "parse", "arguments": { "file": file, "model": "mistral/ocr-latest" } } }))
        .await;
    // A slow call does not block other requests.
    assert_eq!(c.request(2, "ping", json!({})).await["result"], json!({}));
    c.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1, "reason": "test" } }))
        .await;
    assert_eq!(c.request(3, "ping", json!({})).await["id"], 3);

    // A second call reusing an id that is still in flight is refused; the first stays cancellable.
    let call = |id: i64| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                   "params": { "name": "parse", "arguments": { "file": file, "model": "mistral/ocr-latest" } } })
    };
    c.send(call(4)).await;
    c.send(call(4)).await;
    let dup = c.recv().await;
    assert_eq!(dup["error"]["code"], -32600, "{dup}");
    assert!(dup["error"]["message"].as_str().unwrap().contains("already in flight"), "{dup}");
    c.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 4, "reason": "test" } }))
        .await;
    assert_eq!(c.request(5, "ping", json!({})).await["id"], 5);
    // Nothing more arrives for requests 1 or 4.
    let next = tokio::time::timeout(Duration::from_millis(300), c.lines.next_line()).await;
    assert!(next.is_err(), "unexpected message after cancellation: {next:?}");
}

#[tokio::test]
async fn root_confines_local_inputs() {
    let base = std::env::temp_dir().join(format!("puffinparse-mcp-root-{}", std::process::id()));
    let inside = base.join("inside");
    std::fs::create_dir_all(&inside).unwrap();
    let inside_file = inside.join("doc.png");
    let outside_file = base.join("secret.png");
    for f in [&inside_file, &outside_file] {
        std::fs::write(f, b"\x89PNG\r\n\x1a\nnot really a png").unwrap();
    }
    let mut cfg = mock_config().await;
    cfg.root = Some(std::fs::canonicalize(&inside).unwrap());
    let mut c = Client::start(cfg);
    c.initialize("2025-11-25").await;

    let r = c.call(1, "ocr", json!({ "file": inside_file, "model": "mistral/ocr-latest" })).await;
    assert_eq!(r["isError"], false, "{r}");
    let r = c
        .call(2, "ocr", json!({ "file": format!("file://{}", inside_file.display()), "model": "mistral/ocr-latest" }))
        .await;
    assert_eq!(r["isError"], false, "{r}");

    // Outside the root, directly or through `..`, is refused before any provider call.
    for (id, f) in [(3, outside_file.display().to_string()), (4, format!("{}/../secret.png", inside.display()))] {
        let r = c.call(id, "ocr", json!({ "file": f, "model": "mistral/ocr-latest" })).await;
        assert_eq!(r["isError"], true);
        assert!(text(&r).contains("is outside the directory this server may read"), "{r}");
    }
    #[cfg(unix)]
    {
        let link = inside.join("link.png");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        let r = c.call(5, "ocr", json!({ "file": link, "model": "mistral/ocr-latest" })).await;
        assert!(text(&r).contains("is outside the directory"), "symlinks are followed: {r}");
    }
    let r = c.call(6, "ocr", json!({ "file": inside.join("missing.png"), "model": "mistral/ocr-latest" })).await;
    assert!(text(&r).contains("cannot read"), "{r}");
    // URLs are unaffected by --root (the core validates them).
    let r = c.call(7, "compare", json!({ "file": "https://example.com/a.png", "models": ["reducto/nope"] })).await;
    assert!(text(&r).contains("unknown model 'nope'"), "{r}");
    let _ = std::fs::remove_dir_all(&base);
}

// ---- find the best model for a dataset -----------------------------------------------------------

#[tokio::test]
async fn dataset_workflow_end_to_end() {
    let dir = std::env::temp_dir().join(format!("puffinparse-mcp-dataset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scans")).unwrap();
    for name in ["a.png", "b.png", "scans/c.png"] {
        std::fs::write(dir.join(name), b"\x89PNG\r\n\x1a\nnot really a png").unwrap();
    }
    let path = dir.to_string_lossy().into_owned();
    let mut c = Client::start(mock_config().await);
    c.initialize("2025-06-18").await;

    // The guided prompt.
    let prompts = c.request(1, "prompts/list", json!({})).await;
    assert_eq!(prompts["result"]["prompts"][0]["name"], "find_best_model");
    let p = c
        .request(
            2,
            "prompts/get",
            json!({ "name": "find_best_model", "arguments": { "dataset": "/data/docs", "requirements": "5k pages a month" } }),
        )
        .await;
    let msg = p["result"]["messages"][0]["content"]["text"].as_str().unwrap();
    assert!(msg.contains("scan_dataset on my folder (/data/docs)") && msg.contains("record_review"));
    assert!(msg.contains("my needs: 5k pages a month.") && !msg.contains("{REQUIREMENTS}"));
    assert!(c.request(3, "prompts/get", json!({ "name": "nope" })).await["error"].is_object());

    let scan = c.call(4, "scan_dataset", json!({ "path": path })).await;
    assert_eq!(scan["isError"], false, "{scan}");
    assert_eq!(scan["structuredContent"]["documents"], 3);
    assert_eq!(scan["structuredContent"]["total_pages"], 3);

    let rec = c
        .call(5, "recommend_models", json!({ "monthly_pages": 50000, "zdr_required": true, "needs": ["tables"] }))
        .await;
    assert_eq!(rec["isError"], false, "{rec}");
    let cands = rec["structuredContent"]["candidates"].as_array().unwrap();
    assert!(!cands.is_empty());
    assert!(cands.iter().all(|c| c["data_handling"]["zdr"] != "unknown" && c["data_handling"]["zdr"] != "no"));

    let models = json!(["mistral/ocr-latest", "mistral/ocr-2512"]);
    let dry = c.call(6, "run_dataset", json!({ "dataset": path, "models": models, "dry_run": true })).await;
    assert_eq!(dry["isError"], false, "{dry}");
    let est = dry["structuredContent"]["plan"]["estimate_usd"].as_f64().unwrap();
    assert!((est - 0.018).abs() < 1e-9, "{est}");
    // A priced run does not start without the user's limit.
    let refused = c.call(7, "run_dataset", json!({ "dataset": path, "models": models })).await;
    assert_eq!(refused["isError"], true);
    assert!(text(&refused).contains("max_cost_usd"));
    // Models outside the allow-list or the mode are refused before anything runs.
    let bad =
        c.call(8, "run_dataset", json!({ "dataset": path, "models": ["reducto/extract"], "dry_run": true })).await;
    assert_eq!(bad["isError"], true);

    let started = c
        .call(
            9,
            "run_dataset",
            json!({ "dataset": path, "models": models, "max_cost_usd": 1.0, "requirements": { "monthly_pages": 50000 } }),
        )
        .await;
    assert_eq!(started["isError"], false, "{started}");
    let run = started["structuredContent"]["run"].as_str().unwrap().to_string();

    let mut id = 10;
    let status = loop {
        let s = c.call(id, "run_status", json!({ "run": run })).await;
        id += 1;
        if s["structuredContent"]["status"] != "running" {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(id < 300, "run did not finish: {s}");
    };
    assert_eq!(status["structuredContent"]["status"], "complete", "{status}");
    assert_eq!(status["structuredContent"]["progress"]["done"], 6);
    assert_eq!(status["structuredContent"]["progress"]["failed"], 3);

    let analysis = c.call(id, "analyze_run", json!({ "run": run })).await;
    id += 1;
    assert_eq!(analysis["isError"], false, "{analysis}");
    let per_model = analysis["structuredContent"]["per_model"].as_array().unwrap();
    let ok_model = per_model.iter().find(|m| m["model"] == "mistral/ocr-latest").unwrap();
    assert_eq!(ok_model["ok"], 3);
    assert_eq!(ok_model["monthly_usd"].as_f64(), Some(200.0));
    let queue = analysis["structuredContent"]["review_queue"].as_array().unwrap();
    assert_eq!(queue.len(), 3, "every document has a failure");
    let doc = queue[0]["doc"].as_str().unwrap().to_string();

    let out = c.call(id, "get_output", json!({ "run": run, "doc": doc })).await;
    id += 1;
    assert_eq!(out["isError"], false, "{out}");
    let outs = out["structuredContent"]["outputs"].as_array().unwrap();
    assert_eq!(outs[0]["ok"], true);
    assert!(!outs[0]["markdown"].as_str().unwrap().is_empty());
    assert_eq!(outs[1]["ok"], false);

    let review = c
        .call(
            id,
            "record_review",
            json!({ "run": run, "doc": doc, "model": "mistral/ocr-latest", "verdict": "correct" }),
        )
        .await;
    id += 1;
    assert_eq!(review["isError"], false, "{review}");
    let report = c
        .call(id, "analyze_run", json!({ "run": run, "write_report": true, "conclusion": "Use mistral/ocr-latest." }))
        .await;
    id += 1;
    assert!(std::fs::read_to_string(std::path::Path::new(&run).join("report.md"))
        .unwrap()
        .contains("Use mistral/ocr-latest."));
    assert!(text(&report).contains("| 1/1 |"), "{}", text(&report));

    let viewer = c.call(id, "open_viewer", json!({ "run": run })).await;
    id += 1;
    let url = viewer["structuredContent"]["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:"));
    let again = c.call(id, "open_viewer", json!({ "run": run })).await;
    assert_eq!(again["structuredContent"]["url"], url.as_str(), "one viewer per run");
    let page = reqwest::get(&url).await.unwrap();
    assert_eq!(page.status(), 200);

    // No key or path ever leaks into the transcript.
    assert!(!c.transcript.iter().any(|l| l.contains(TEST_KEY)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dataset_tools_respect_root() {
    let inside = std::env::temp_dir().join(format!("puffinparse-mcp-root-in-{}", std::process::id()));
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("a.png"), b"\x89PNG\r\n\x1a\nx").unwrap();
    let cfg = Config { root: Some(std::fs::canonicalize(&inside).unwrap()), ..Config::default() };
    let mut c = Client::start(cfg);
    c.initialize("2025-06-18").await;
    let ok = c.call(1, "scan_dataset", json!({ "path": inside })).await;
    assert_eq!(ok["isError"], false, "{ok}");
    let outside = c.call(2, "scan_dataset", json!({ "path": std::env::temp_dir() })).await;
    assert_eq!(outside["isError"], true);
    assert!(text(&outside).contains("--root"));
    let anon = c
        .call(3, "anonymize_dataset", json!({ "dataset": inside, "out": std::env::temp_dir().join("pp-anon-outside") }))
        .await;
    assert_eq!(anon["isError"], true);
    let _ = std::fs::remove_dir_all(&inside);
}
