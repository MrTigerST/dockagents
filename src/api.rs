//! REST API for orchestrators (`dockagents serve`).
//!
//! Mirrors the request/response shape in dockagents.md §8.2:
//!
//!   POST /invoke                         buffered JSON response
//!   POST /invoke?stream=true             text/event-stream with progress
//!                                        events (`run_started`,
//!                                        `agent_spawned`, `agent_finished`,
//!                                        `run_finished`)
//!
//!   GET  /                  service info
//!   GET  /sandboxes         list installed sandboxes
//!   GET  /sandboxes/:name   manifest for a single sandbox
//!
//! Implemented on top of `tiny_http`. Streaming uses chunked transfer
//! encoding by passing a custom `Read` to `Response::new` whose source is a
//! progress channel drained in-thread.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver};
use serde_json::json;
use tiny_http::{Method, Request, Response, Server};

use crate::manifest::Manifest;
use crate::paths;
use crate::runtime::{self, Input, ProgressSink, RunEvent};

pub fn serve(host: &str, port: u16) -> Result<()> {
    let addr = format!("{host}:{port}");
    let server = Server::http(&addr)
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    println!("dockagents REST API listening on http://{addr}");

    for mut req in server.incoming_requests() {
        let path = req.url().to_string();
        let method = req.method().clone();

        // Streaming variant: take ownership of the request, write chunks
        // straight from the progress channel, and skip the JSON dispatcher.
        if method == Method::Post
            && path.starts_with("/invoke")
            && wants_stream(&path)
        {
            if let Err(e) = handle_invoke_stream(req) {
                tracing::warn!("streaming invoke failed: {e:#}");
            }
            continue;
        }

        let result = route(&method, &path, &mut req);
        let resp = match result {
            Ok(value) => Response::from_string(serde_json::to_string_pretty(&value)?)
                .with_header(json_header())
                .with_status_code(200),
            Err(ApiError { status, message }) => {
                let body = serde_json::to_string_pretty(&json!({ "error": message }))?;
                Response::from_string(body)
                    .with_header(json_header())
                    .with_status_code(status)
            }
        };
        if let Err(e) = req.respond(resp) {
            tracing::warn!("response failed: {e}");
        }
    }

    Ok(())
}

struct ApiError {
    status: u16,
    message: String,
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(value: E) -> Self {
        Self {
            status: 500,
            message: value.to_string(),
        }
    }
}

fn route(
    method: &Method,
    path: &str,
    req: &mut Request,
) -> std::result::Result<serde_json::Value, ApiError> {
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        (Method::Get, "/") => Ok(json!({
            "service": "dockagents",
            "version": env!("CARGO_PKG_VERSION"),
            "endpoints": [
                "GET  /sandboxes",
                "GET  /sandboxes/:name",
                "POST /invoke",
            ],
        })),
        (Method::Get, "/sandboxes") => list_sandboxes().map_err(into_api_error),
        (Method::Get, p) if p.starts_with("/sandboxes/") => {
            let name = &p["/sandboxes/".len()..];
            get_sandbox(name).map_err(into_api_error)
        }
        (Method::Post, "/invoke") => invoke(req).map_err(into_api_error),
        _ => Err(ApiError {
            status: 404,
            message: format!("not found: {} {}", method, path),
        }),
    }
}

fn into_api_error(e: anyhow::Error) -> ApiError {
    ApiError {
        status: 500,
        message: format!("{e:#}"),
    }
}

fn list_sandboxes() -> Result<serde_json::Value> {
    let dir = paths::sandboxes_dir()?;
    if !dir.exists() {
        return Ok(json!({ "sandboxes": [] }));
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.yaml");
        if !manifest_path.exists() {
            continue;
        }
        if let Ok(m) = Manifest::load(&manifest_path) {
            out.push(json!({
                "name": m.name,
                "version": m.version.to_string(),
                "description": m.description,
                "lifecycle": m.lifecycle,
                "execution": { "mode": m.execution.mode },
                "agents": m.agents.iter().map(|a| json!({
                    "id": a.id,
                    "model": a.model,
                })).collect::<Vec<_>>(),
            }));
        }
    }
    Ok(json!({ "sandboxes": out }))
}

fn get_sandbox(name: &str) -> Result<serde_json::Value> {
    let install = paths::sandbox_dir(name)?;
    if !install.exists() {
        return Err(anyhow::anyhow!("sandbox '{name}' is not installed"));
    }
    let m = Manifest::load(&install.join("manifest.yaml"))?;
    Ok(serde_json::to_value(m)?)
}

fn invoke(req: &mut Request) -> Result<serde_json::Value> {
    let mut body = String::new();
    req.as_reader().read_to_string(&mut body).context("reading body")?;
    let payload: serde_json::Value =
        serde_json::from_str(&body).context("parsing JSON body")?;

    let name = payload
        .get("sandbox")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing `sandbox`"))?;
    let install = paths::sandbox_dir(name)?;
    let manifest = Manifest::load(&install.join("manifest.yaml"))?;

    let input = Input {
        path: payload
            .get("input_path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from),
        text: payload
            .get("input")
            .map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            }),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let report = runtime::run_sandbox(&install, manifest, input, cancel)?;
    let report_text = std::fs::read_to_string(&report.output_path).unwrap_or_default();

    Ok(json!({
        "sandbox": report.sandbox,
        "version": report.version,
        "execution_time_ms": report.execution_time_ms,
        "output": {
            "report_md": report_text,
            "report_path": report.output_path,
            "agents": report.agent_outputs,
        },
    }))
}

fn json_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn wants_stream(path: &str) -> bool {
    let q = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    q.split('&').any(|kv| kv == "stream=true" || kv == "stream=1")
}

fn error_resp(status: u16, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string_pretty(&json!({ "error": message })).unwrap_or_default();
    Response::from_string(body)
        .with_header(json_header())
        .with_status_code(status)
}

/// `Read` source that pulls SSE-formatted bytes from a progress channel.
/// `tiny_http` consumes this with a `None` content-length so the response is
/// transferred chunked, which is exactly what SSE wants.
struct SseSource {
    rx: Receiver<RunEvent>,
    leftover: Vec<u8>,
    cursor: usize,
    done: bool,
}

impl SseSource {
    fn new(rx: Receiver<RunEvent>) -> Self {
        // Prime the stream with a comment frame so the client gets bytes
        // immediately and any intermediary flushes its buffer.
        Self {
            rx,
            leftover: b": dockagents-stream\n\n".to_vec(),
            cursor: 0,
            done: false,
        }
    }

    fn refill(&mut self) -> std::io::Result<bool> {
        if self.done {
            return Ok(false);
        }
        let event = match self.rx.recv() {
            Ok(e) => e,
            Err(_) => {
                // Sender dropped — emit a terminal `event: end` and finish.
                self.leftover = b"event: end\ndata: {}\n\n".to_vec();
                self.cursor = 0;
                self.done = true;
                return Ok(true);
            }
        };
        let name = match &event {
            RunEvent::RunStarted { .. } => "run_started",
            RunEvent::AgentSpawned { .. } => "agent_spawned",
            RunEvent::AgentFinished { .. } => "agent_finished",
            RunEvent::RunFinished { .. } => "run_finished",
        };
        let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        let frame = format!("event: {name}\ndata: {payload}\n\n");
        self.leftover = frame.into_bytes();
        self.cursor = 0;
        Ok(true)
    }
}

impl Read for SseSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cursor >= self.leftover.len() && !self.refill()? {
            return Ok(0);
        }
        let available = self.leftover.len() - self.cursor;
        let n = available.min(buf.len());
        buf[..n].copy_from_slice(&self.leftover[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

fn handle_invoke_stream(mut req: Request) -> Result<()> {
    let mut body = String::new();
    if let Err(e) = req.as_reader().read_to_string(&mut body) {
        let _ = req.respond(error_resp(400, &format!("reading body: {e}")));
        return Ok(());
    }
    let payload: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            let _ = req.respond(error_resp(400, &format!("invalid JSON: {e}")));
            return Ok(());
        }
    };
    let name = match payload.get("sandbox").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            let _ = req.respond(error_resp(400, "missing `sandbox`"));
            return Ok(());
        }
    };

    let install = paths::sandbox_dir(&name)?;
    let manifest = match Manifest::load(&install.join("manifest.yaml")) {
        Ok(m) => m,
        Err(e) => {
            let _ = req.respond(error_resp(404, &format!("{e:#}")));
            return Ok(());
        }
    };

    let input = Input {
        path: payload
            .get("input_path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from),
        text: payload
            .get("input")
            .map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            }),
    };

    let (tx, rx): (ProgressSink, Receiver<RunEvent>) = unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel_for_run = cancel.clone();
        thread::spawn(move || {
            let _ = runtime::run_sandbox_with_progress(
                &install,
                manifest,
                input,
                cancel_for_run,
                Some(tx),
            );
            // sender drops → SseSource returns the terminal `event: end`.
        });
    }

    let source = SseSource::new(rx);
    let response = Response::new(
        tiny_http::StatusCode(200),
        vec![
            tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/event-stream"[..],
            )
            .unwrap(),
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..])
                .unwrap(),
            tiny_http::Header::from_bytes(&b"X-Accel-Buffering"[..], &b"no"[..]).unwrap(),
        ],
        source,
        None, // unknown length → chunked transfer
        None,
    );
    if let Err(e) = req.respond(response) {
        tracing::warn!("streaming response failed: {e}");
    }
    Ok(())
}
