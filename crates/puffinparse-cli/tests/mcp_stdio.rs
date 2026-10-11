//! End-to-end: the real `puffinparse mcp` binary over its stdin / stdout. No provider is called.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn mcp_over_real_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_puffinparse"))
        .args(["mcp", "--models", "reducto,mistral/ocr-latest"])
        // Verbose logs must still go to stderr only.
        .env("PUFFINPARSE_LOG", "debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn puffinparse mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut roundtrip = |msg: Value| -> Option<Value> {
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        msg.get("id")?;
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout carried a non-JSON line {line:?}: {e}")))
    };

    let init = roundtrip(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } } }))
    .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert!(roundtrip(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());

    let list = roundtrip(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);

    let models = roundtrip(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "list_models", "arguments": { "mode": "parse" } } }))
    .unwrap();
    let models = &models["result"]["structuredContent"]["models"];
    assert!(models.as_array().unwrap().iter().all(|m| {
        let m = m["model"].as_str().unwrap();
        m.starts_with("reducto/") || m == "mistral/ocr-latest"
    }));

    let discover = roundtrip(json!({ "jsonrpc": "2.0", "id": "d", "method": "server/discover",
        "params": { "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                               "io.modelcontextprotocol/clientCapabilities": {} } } }))
    .unwrap();
    assert_eq!(discover["result"]["resultType"], "complete");

    // Closing stdin shuts the server down cleanly with nothing else on stdout.
    drop(stdin);
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    assert!(rest.is_empty(), "unexpected trailing stdout: {rest:?}");
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn mcp_rejects_a_bad_allow_list_at_startup() {
    let out = Command::new(env!("CARGO_BIN_EXE_puffinparse"))
        .args(["mcp", "--models", "reducto/nope"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown model 'nope'"));
}
