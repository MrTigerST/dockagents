//! MCP (Model Context Protocol) server — JSON-RPC over stdio.
//!
//! Exposes installed sandboxes as MCP tools so any MCP-compatible client
//! (Claude Desktop, Cursor, custom orchestrators) can discover and invoke
//! them. Mirrors the contract in dockagents.md §8.2.
//!
//! Protocol: line-delimited JSON-RPC 2.0 on stdin/stdout.
//!
//! Methods we implement:
//!   * `initialize`       capability handshake
//!   * `tools/list`       returns one MCP tool per installed sandbox
//!   * `tools/call`       runs the named sandbox and returns its synthesized output
//!
//! Methods that are accepted but no-ops:
//!   * `notifications/initialized`
//!   * `ping`

use std::io::{BufRead, BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde_json::{json, Value};

use crate::manifest::Manifest;
use crate::paths;
use crate::runtime::{self, Input};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("stdin read failed: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_response(&mut out, &error_response(Value::Null, -32700, &format!("parse error: {e}")))?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications have no `id` and never get a reply.
        let is_notification = req.get("id").is_none();

        let result = handle_method(method, &params);

        if is_notification {
            continue;
        }
        match result {
            Ok(value) => write_response(&mut out, &json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value,
            }))?,
            Err((code, message)) => {
                write_response(&mut out, &error_response(id, code, &message))?
            }
        }
    }

    Ok(())
}

fn handle_method(method: &str, params: &Value) -> std::result::Result<Value, (i32, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "dockagents", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list().map_err(|e| (-32603, e.to_string()))?),
        "tools/call" => Ok(tools_call(params).map_err(|e| (-32603, e.to_string()))?),
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

fn tools_list() -> Result<Value> {
    let mut tools = Vec::new();
    let dir = paths::sandboxes_dir()?;
    if !dir.exists() {
        return Ok(json!({ "tools": tools }));
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.yaml");
        if !manifest_path.exists() {
            continue;
        }
        let m = match Manifest::load(&manifest_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        tools.push(json!({
            "name": m.name,
            "description": format!(
                "{} (lifecycle: {:?}, mode: {:?}, agents: {})",
                if m.description.is_empty() { "DockAgents sandbox" } else { &m.description },
                m.lifecycle,
                m.execution.mode,
                m.agents.len(),
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "Inline text passed to every agent in this sandbox.",
                    },
                    "input_path": {
                        "type": "string",
                        "description": "Optional host path of a file or directory the agents should consume.",
                    },
                },
            },
        }));
    }
    Ok(json!({ "tools": tools }))
}

fn tools_call(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("tools/call: missing `name`"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let install_root = paths::sandbox_dir(name)?;
    let manifest = Manifest::load(&install_root.join("manifest.yaml"))?;

    let input = Input {
        path: args
            .get("input_path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from),
        text: args
            .get("input")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let report = runtime::run_sandbox(&install_root, manifest, input, cancel)?;
    let report_text = std::fs::read_to_string(&report.output_path).unwrap_or_default();

    Ok(json!({
        "content": [
            { "type": "text", "text": report_text }
        ],
        "isError": false,
        "_meta": {
            "sandbox": report.sandbox,
            "version": report.version,
            "execution_time_ms": report.execution_time_ms,
            "output_path": report.output_path,
            "agent_outputs": report.agent_outputs,
        }
    }))
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn write_response<W: Write>(w: &mut W, v: &Value) -> Result<()> {
    let line = serde_json::to_string(v)?;
    writeln!(w, "{line}")?;
    w.flush()?;
    Ok(())
}
