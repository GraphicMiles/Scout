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
use tokio::io::AsyncBufReadExt;

const MAX_MCP_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Read one newline-delimited frame from `reader`, capping memory at `max_bytes`.
/// Returns `Ok(None)` on clean EOF, `Ok(Some(Ok(line)))` for a normal frame, or
/// `Ok(Some(Err(())))` after draining an oversized frame to its terminating `\n`.
pub(crate) async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<std::result::Result<String, ()>>> {
    let mut buf = Vec::new();
    let mut over = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() && !over {
                return Ok(None);
            }
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if !over {
                if buf.len() + pos + 1 <= max_bytes {
                    buf.extend_from_slice(&available[..=pos]);
                } else {
                    over = true;
                }
            }
            reader.consume(pos + 1);
            break;
        } else {
            let len = available.len();
            if !over {
                if buf.len() + len <= max_bytes {
                    buf.extend_from_slice(available);
                } else {
                    over = true;
                    buf.clear();
                }
            }
            reader.consume(len);
        }
    }
    if over {
        return Ok(Some(Err(())));
    }
    String::from_utf8(buf)
        .map(|s| Some(Ok(s)))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Run the MCP stdio bridge until stdin closes.
pub async fn run() -> Result<(), ScoutError> {
    // Loopback only, port assigned by the OS, one ephemeral key held in
    // memory: build from an owned map rather than mutating process env vars.
    let key = crate::util::random_id("mcp_", 24);
    let mut env_map: std::collections::HashMap<String, String> = std::env::vars().collect();
    env_map.insert("SCOUT_API_KEYS".to_string(), key.clone());
    let mut cfg = crate::config::Config::from_map(env_map)
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
    loop {
        let frame = read_bounded_line(&mut stdin, MAX_MCP_LINE_BYTES)
            .await
            .map_err(|e| ScoutError::new(ErrorCode::Internal, format!("stdin read failed: {e}")))?;
        let line = match frame {
            None => {
                // EOF: the client went away. Shut down.
                server_task.abort();
                let _ = scout;
                return Ok(());
            }
            Some(Ok(s)) => s,
            Some(Err(())) => {
                let _ = writeln!(stdout, "{}", json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": { "code": -32600, "message": "Invalid Request: frame exceeds maximum length" }
                }));
                let _ = stdout.flush();
                continue;
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_line_reader_rejects_oversized_frame_and_recovers() {
        let payload = format!("{}\n{{\"id\":1}}\n", "x".repeat(256));
        let mut reader = tokio::io::BufReader::new(payload.as_bytes());
        let first = read_bounded_line(&mut reader, 64).await.unwrap();
        assert!(matches!(first, Some(Err(()))));
        let second = read_bounded_line(&mut reader, 64).await.unwrap();
        assert_eq!(second.unwrap().unwrap(), "{\"id\":1}\n");
        let eof = read_bounded_line(&mut reader, 64).await.unwrap();
        assert!(eof.is_none());
    }
}
