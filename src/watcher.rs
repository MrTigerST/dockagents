//! File-watcher driven re-runs.
//!
//! `dockagents watch <name>` watches the host side of the first `readwrite`
//! mount declared by the sandbox (typically `~/Desktop/<name>/`) and re-runs
//! the sandbox whenever a file under that path changes. Drops events through
//! a debouncer so an editor saving in bursts doesn't trigger N runs.
//!
//! This is the "drop a file in a folder, results appear" UX described in
//! dockagents.md §9.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};

use crate::manifest::{Manifest, MountMode};
use crate::paths;
use crate::runtime::{self, Input};

pub fn watch(target: &str, debounce_str: &str) -> Result<()> {
    let install = paths::sandbox_dir(target)?;
    let manifest_path = install.join("manifest.yaml");
    if !manifest_path.exists() {
        return Err(anyhow!(
            "sandbox '{target}' is not installed (no manifest at {})",
            manifest_path.display()
        ));
    }
    let manifest = Manifest::load(&manifest_path)?;

    let mount = manifest
        .mounts
        .iter()
        .find(|m| matches!(m.mode, MountMode::Readwrite))
        .ok_or_else(|| anyhow!("sandbox '{target}' has no readwrite mount to watch"))?;
    let host = paths::expand(&mount.host)?;
    std::fs::create_dir_all(&host)?;
    let watch_dir = host.join("input");
    std::fs::create_dir_all(&watch_dir)?;

    let debounce = humantime::parse_duration(debounce_str)
        .with_context(|| format!("parsing --debounce '{debounce_str}'"))?;

    println!("watching {} (debounce {:?})", watch_dir.display(), debounce);

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(debounce, move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| anyhow!("creating debouncer: {e}"))?;
    debouncer
        .watcher()
        .watch(&watch_dir, RecursiveMode::Recursive)
        .map_err(|e| anyhow!("watch failed: {e}"))?;

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let c = cancel.clone();
        let _ = ctrlc::set_handler(move || {
            c.store(true, Ordering::Relaxed);
        });
    }

    while !cancel.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) if !events.is_empty() => {
                let names: Vec<String> = events
                    .iter()
                    .map(|e| e.path.display().to_string())
                    .collect();
                println!("change detected: {}", names.join(", "));
                if let Err(e) = run_once(&install, &manifest, &watch_dir, cancel.clone()) {
                    eprintln!("run failed: {e:#}");
                }
            }
            Ok(Err(errs)) => {
                tracing::warn!("watch error(s): {errs:?}");
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn run_once(
    install: &std::path::Path,
    manifest: &Manifest,
    watch_dir: &PathBuf,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let input = Input {
        path: Some(watch_dir.clone()),
        text: None,
    };
    let report = runtime::run_sandbox(install, manifest.clone(), input, cancel)?;
    println!(
        "  → run completed in {}ms ({} agents). output: {}",
        report.execution_time_ms,
        report.agent_outputs.len(),
        report.output_path.display()
    );
    Ok(())
}
