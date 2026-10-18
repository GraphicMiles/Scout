//! MCP over stdio.
//!
//! Model Context Protocol clients speak newline-delimited JSON-RPC 2.0 over a
//! process's stdin and stdout. Rather than reimplementing the HTTP surface
//! inside this module, the `scout mcp` subcommand starts the same HTTP server
//! on a loopback port only it can reach, with an ephemeral key, and bridges
//! every tool call to it. One surface, two doors; nothing can drift.

use crate::error::{ErrorCode, ScoutError};
use crate::server;
use serde_json::{json, Value};
use std::io::Write;

/// Run the MCP stdio bridge until stdin closes.
pub async fn run() -> Result<(), ScoutError> {
    // Loopback only, port assigned by the OS, one ephemeral key held in
    // memory: the server is private to this process. The key is planted in the
    // environment before config parsing so the "keys required" check passes
    // without the operator supplying any.
    let key = crate::util::random_id("mcp_", 24);
    std::env::set_var("SCOUT_API_KEYS", &key);
    let mut cfg = crate::config::Config::from_env()
        .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("config error: {e}")))?;
    cfg.host = "127.0.0.1".to_string();
    cfg.port = 0;
    // Logs go to stderr already; keep them quiet so terminal noise stays low.
    cfg.log_level = "error".to_string();

    let (scout, server_task, addr) = server::start(cfg).await?;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Announce on stderr, never stdout: stdout is the protocol stream.
    eprintln!(
        "{{\"level\":\"info\",\"msg\":\"scout mcp bridge\",\"port\":{}}}",
        addr.port()
    );

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let n = tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line)
            .await
            .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("stdin read failed: {e}")))?;
        if n == 0 {
            // EOF: the client went away. Shut down.
            server_task.abort();
            let _ = scout;
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                // A malformed line gets the JSON-RPC parse error, then we go on.
                let _ = writeln!(stdout, "{}", json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": { "code": -32700, "message": "Parse error" }
                }));
                let _ = stdout.flush();
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // Notifications carry no id and get no response.
        let Some(id) = id else {
            continue;
        };

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2024-11-05"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "scout", "version": scout.cfg.version },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let tools = http(&client, &base, &key, reqwest::Method::GET, "/v1/tools", None)
                    .await?;
                Ok(json!({ "tools": tools.get("tools").cloned().unwrap_or(json!([])) }))
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                match http(
                    &client,
                    &base,
                    &key,
                    reqwest::Method::POST,
                    "/v1/tools/call",
                    Some(json!({ "name": name, "arguments": arguments })),
                )
                .await
                {
                    Ok(v) => Ok(json!({
                        "content": [{ "type": "text", "text": v.to_string() }],
                        "isError": false,
                    })),
                    Err(e) => Ok(json!({
                        "content": [{ "type": "text", "text": e.to_json().to_string() }],
                        "isError": true,
                    })),
                }
            }
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            other => Err(format!("Method not found: {other}")),
        };

        let response = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(msg) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": msg }
            }),
        };
        // One line per message, flushed: MCP stdio is line-delimited.
        let _ = writeln!(stdout, "{}", response);
        let _ = stdout.flush();
    }
}

/// One authenticated round trip to the internal server.
async fn http(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, ScoutError> {
    let mut req = client
        .request(method, format!("{base}{path}"))
        .bearer_auth(key);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("bridge request failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("bridge read failed: {e}")))?;
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    if status != 200 {
        let err = value.get("error").cloned().unwrap_or(json!({}));
        return Err(ScoutError::new(
            ErrorCode::from_status(status),
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("tool call failed"),
        ));
    }
    Ok(value)
}
