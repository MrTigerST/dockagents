//! Agent subprocess (`dockagents __agent`).
//!
//! Reads its [`AgentConfig`] from stdin, loads its skill markdown, gathers any
//! input from its workspace, calls the LLM endpoint declared in the manifest,
//! and writes the model's response to its `output_file`.
//!
//! Bus traffic uses two channels:
//!   * `stdout` — lines prefixed with `@@BUS@@ ` are JSON `Envelope`s the
//!     runtime forwards to other agents.
//!   * `<workspace>/.inbox.jsonl` — the runtime appends inbound envelopes here;
//!     the agent reads them at start-of-turn.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime::process::{AgentConfig, LlmEndpoint};

/// Entry point for the spawned agent process.
pub fn run() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let cfg: AgentConfig = serde_json::from_str(raw.trim())
        .context("parsing agent config from stdin")?;

    log_line(&cfg, &format!("agent {} starting (model={})", cfg.agent_id, cfg.model))?;

    // Announce subscriptions for directed-topology bus.
    if !cfg.subscribes.is_empty() {
        publish_envelope(&cfg.agent_id, None, "__subscribe__", &cfg.subscribes.join(","), false);
    }

    let skill = std::fs::read_to_string(&cfg.skill_path)
        .with_context(|| format!("reading skill at {}", cfg.skill_path.display()))?;
    let user_input = collect_input(&cfg.input_dir)?;
    let inbox = drain_inbox(&cfg.workspace.join(".inbox.jsonl"))?;

    let prompt = build_user_prompt(&user_input, &inbox);
    log_line(&cfg, &format!("user prompt size: {} bytes", prompt.len()))?;

    let response = match call_llm(&cfg, &skill, &prompt) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("LLM call failed for agent '{}': {e:#}", cfg.agent_id);
            log_line(&cfg, &msg)?;
            // Write a placeholder so the synthesized report still reflects what
            // happened, then exit non-zero.
            if let Some(parent) = cfg.output_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &cfg.output_file,
                format!("# {} — error\n\n{msg}\n", cfg.agent_id),
            )?;
            publish_envelope(&cfg.agent_id, None, "error", &msg, true);
            return Err(e);
        }
    };

    if let Some(parent) = cfg.output_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&cfg.output_file, &response)
        .with_context(|| format!("writing output to {}", cfg.output_file.display()))?;
    log_line(&cfg, &format!("output written ({} bytes)", response.len()))?;

    // Tell the bus we're done — flushes any post-output buffered envelopes.
    publish_envelope(
        &cfg.agent_id,
        None,
        "output",
        &summarize_for_bus(&response),
        true,
    );

    Ok(())
}

fn build_user_prompt(input: &str, inbox: &[BusInbound]) -> String {
    let mut out = String::new();
    if !input.is_empty() {
        out.push_str("=== INPUT ===\n");
        out.push_str(input);
        out.push_str("\n");
    }
    if !inbox.is_empty() {
        out.push_str("\n=== MESSAGES FROM OTHER AGENTS ===\n");
        for msg in inbox {
            out.push_str(&format!("[{}] {}: {}\n", msg.from, msg.topic, msg.body));
        }
    }
    if out.is_empty() {
        out.push_str("(no input provided)");
    }
    out
}

fn summarize_for_bus(text: &str) -> String {
    let trimmed: String = text.chars().take(280).collect();
    trimmed
}

fn collect_input(dir: &Path) -> Result<String> {
    if !dir.exists() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for entry in walkdir::WalkDir::new(dir).max_depth(2) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let path = entry.path();
            if let Ok(text) = std::fs::read_to_string(path) {
                out.push_str(&format!(
                    "\n--- {} ---\n{}\n",
                    path.strip_prefix(dir).unwrap_or(path).display(),
                    text
                ));
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize, Serialize)]
struct BusInbound {
    from: String,
    #[serde(default)]
    to: Option<String>,
    topic: String,
    body: String,
    #[serde(default)]
    output_ready: bool,
}

fn drain_inbox(path: &Path) -> Result<Vec<BusInbound>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    for line in body.lines() {
        if let Ok(env) = serde_json::from_str::<BusInbound>(line) {
            out.push(env);
        }
    }
    // Truncate so a future iteration doesn't reread.
    let _ = std::fs::write(path, "");
    Ok(out)
}

fn publish_envelope(from: &str, to: Option<&str>, topic: &str, body: &str, output_ready: bool) {
    let env = serde_json::json!({
        "from": from,
        "to": to,
        "topic": topic,
        "body": body,
        "output_ready": output_ready,
    });
    println!("@@BUS@@ {}", env);
    let _ = std::io::stdout().flush();
}

fn log_line(cfg: &AgentConfig, msg: &str) -> Result<()> {
    if let Some(parent) = cfg.log_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.log_file)?;
    writeln!(f, "[{}] {}", chrono::Utc::now().to_rfc3339(), msg)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// LLM dispatch
// ─────────────────────────────────────────────────────────────────────────────

fn call_llm(cfg: &AgentConfig, skill: &str, user: &str) -> Result<String> {
    match cfg.llm.provider.as_str() {
        "anthropic" => call_anthropic(&cfg.llm, &cfg.model, cfg.temperature, skill, user, cfg.timeout_secs),
        "openai" | "openai-compatible" => {
            call_openai(&cfg.llm, &cfg.model, cfg.temperature, skill, user, cfg.timeout_secs)
        }
        other => Err(anyhow!(
            "unknown LLM provider '{other}' (expected anthropic, openai, or openai-compatible)"
        )),
    }
}

fn build_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_secs.max(30)))
        .build()
}

fn call_anthropic(
    llm: &LlmEndpoint,
    model: &str,
    temperature: Option<f32>,
    skill: &str,
    user: &str,
    timeout_secs: u64,
) -> Result<String> {
    let agent = build_agent(timeout_secs);
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": llm.max_tokens,
        "system": skill,
        "messages": [{ "role": "user", "content": user }],
    });
    if let Some(t) = temperature {
        body["temperature"] = serde_json::json!(t);
    }

    let mut req = agent
        .post(&llm.endpoint)
        .set("x-api-key", &llm.api_key)
        .set(
            "anthropic-version",
            llm.api_version.as_deref().unwrap_or("2023-06-01"),
        )
        .set("content-type", "application/json");
    for (k, v) in &llm.extra_headers {
        req = req.set(k, v);
    }

    let resp = req
        .send_json(body)
        .map_err(|e| anyhow!("anthropic request failed: {e}"))?;
    let parsed: serde_json::Value = resp.into_json()?;
    let content = parsed
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("anthropic response missing `content`: {parsed}"))?;
    let mut out = String::new();
    for block in content {
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            out.push_str(text);
        }
    }
    if out.is_empty() {
        return Err(anyhow!("anthropic returned no text content: {parsed}"));
    }
    Ok(out)
}

fn call_openai(
    llm: &LlmEndpoint,
    model: &str,
    temperature: Option<f32>,
    skill: &str,
    user: &str,
    timeout_secs: u64,
) -> Result<String> {
    let agent = build_agent(timeout_secs);
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": llm.max_tokens,
        "messages": [
            { "role": "system", "content": skill },
            { "role": "user", "content": user }
        ],
    });
    if let Some(t) = temperature {
        body["temperature"] = serde_json::json!(t);
    }

    let mut req = agent
        .post(&llm.endpoint)
        .set("authorization", &format!("Bearer {}", llm.api_key))
        .set("content-type", "application/json");
    for (k, v) in &llm.extra_headers {
        req = req.set(k, v);
    }

    let resp = req
        .send_json(body)
        .map_err(|e| anyhow!("openai request failed: {e}"))?;
    let parsed: serde_json::Value = resp.into_json()?;
    let choices = parsed
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("openai response missing `choices`: {parsed}"))?;
    let text = choices
        .first()
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("openai response missing message content: {parsed}"))?;
    Ok(text.to_string())
}
