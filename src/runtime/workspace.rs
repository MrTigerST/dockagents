//! Sandbox filesystem layout, agent workspaces, and host mounts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use fs_extra::dir::{CopyOptions, copy as copy_dir};

use crate::manifest::{Lifecycle, Manifest, MountMode};
use crate::paths;

use super::{AgentOutput, Input};

/// All host paths needed to run a sandbox.
pub struct SandboxLayout {
    pub install_root: PathBuf,
    pub run_root: PathBuf,
    pub workspaces: HashMap<String, PathBuf>,
    pub state_dir: PathBuf,
    pub output_dir: PathBuf,
}

impl SandboxLayout {
    /// Materialize the on-disk layout under `~/.dockagents/sandboxes/<name>/`
    /// (persistent) or under `~/.dockagents/cache/<name>/<run-id>/` (ephemeral).
    pub fn prepare(install_root: &Path, manifest: &Manifest) -> Result<Self> {
        let run_root = match manifest.lifecycle {
            Lifecycle::Persistent => paths::sandbox_dir(&manifest.name)?,
            Lifecycle::Ephemeral => {
                let id = chrono::Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
                paths::cache_dir()?.join(&manifest.name).join(id)
            }
        };

        // Seed the run root from the install source if it's not already
        // pointing at it. For persistent sandboxes that have already been
        // installed, `install_root == run_root` and this is a no-op.
        if install_root.canonicalize().ok() != run_root.canonicalize().ok() {
            seed_from(install_root, &run_root)?;
        }

        let mut workspaces = HashMap::new();
        for agent in &manifest.agents {
            let ws = run_root.join(&agent.workspace);
            std::fs::create_dir_all(&ws).with_context(|| {
                format!("creating workspace for agent {}: {}", agent.id, ws.display())
            })?;
            std::fs::create_dir_all(ws.join("input"))?;
            std::fs::create_dir_all(ws.join("output"))?;
            workspaces.insert(agent.id.clone(), ws);
        }

        let state_dir = run_root.join(".state");
        std::fs::create_dir_all(&state_dir)?;
        let output_dir = run_root.join("output");
        std::fs::create_dir_all(&output_dir)?;

        Ok(Self {
            install_root: install_root.to_path_buf(),
            run_root,
            workspaces,
            state_dir,
            output_dir,
        })
    }

    /// Honor the `mounts:` block from the manifest by ensuring host
    /// directories exist and creating per-agent symlinks/dir entries that
    /// point at them.
    ///
    /// Real OS-level enforcement of `readonly` (Bubblewrap on Linux,
    /// Seatbelt on macOS) is Phase 1 follow-up work; for now we create
    /// the bridges and rely on convention.
    pub fn bridge_mounts(&self, manifest: &Manifest) -> Result<()> {
        for mount in &manifest.mounts {
            let host = paths::expand(&mount.host)?;
            std::fs::create_dir_all(&host)
                .with_context(|| format!("creating mount host {}", host.display()))?;

            let inside = self.sandbox_mount_path(&mount.sandbox);
            if let Some(parent) = inside.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Cross-platform "bridge" without symlinks: write a small
            // `mount.json` next to the sandbox-side path so agents (and tests)
            // can read where the host folder lives. Phase 1 follow-up: real
            // bind mounts on Linux/macOS.
            std::fs::create_dir_all(&inside)?;
            let pointer = inside.join(".mount.json");
            let info = serde_json::json!({
                "host": host,
                "mode": match mount.mode { MountMode::Readonly => "readonly", MountMode::Readwrite => "readwrite" },
            });
            std::fs::write(&pointer, serde_json::to_vec_pretty(&info)?)?;

            // Mirror the most-recent contents of the host into the sandbox
            // path on `readwrite` mounts so agents can read/write through it
            // by touching `inside/...`.
            if matches!(mount.mode, MountMode::Readwrite) {
                mirror_dir(&host, &inside).ok();
            } else {
                mirror_dir(&host, &inside).ok();
            }
        }
        Ok(())
    }

    fn sandbox_mount_path(&self, mount_path: &Path) -> PathBuf {
        let trimmed = mount_path
            .strip_prefix("/")
            .unwrap_or(mount_path)
            .to_path_buf();
        self.run_root.join("mounts").join(trimmed)
    }

    /// Copy / symlink the user-provided `--input` argument into each agent
    /// workspace's `input/` directory.
    pub fn distribute_input(&self, manifest: &Manifest, input: &Input) -> Result<()> {
        for agent in &manifest.agents {
            let ws = self
                .workspaces
                .get(&agent.id)
                .ok_or_else(|| anyhow!("workspace missing for {}", agent.id))?;
            let dest = ws.join("input");
            if let Some(text) = &input.text {
                std::fs::write(dest.join("input.txt"), text)?;
            }
            if let Some(path) = &input.path {
                if path.is_file() {
                    let name = path
                        .file_name()
                        .ok_or_else(|| anyhow!("input path has no filename"))?;
                    std::fs::copy(path, dest.join(name))?;
                } else if path.is_dir() {
                    let mut opts = CopyOptions::new();
                    opts.copy_inside = true;
                    opts.overwrite = true;
                    copy_dir(path, &dest, &opts)?;
                }
            }
        }
        Ok(())
    }

    pub fn agent_output_file(&self, agent_id: &str) -> PathBuf {
        self.workspaces
            .get(agent_id)
            .cloned()
            .unwrap_or_else(|| self.run_root.clone())
            .join("output")
            .join("output.md")
    }

    pub fn log_file(&self, agent_id: &str) -> PathBuf {
        self.state_dir.join(format!("{agent_id}.log"))
    }

    /// Write a synthesized markdown report aggregating each agent's output.
    pub fn write_synthesized_report(
        &self,
        manifest: &Manifest,
        outputs: &HashMap<String, AgentOutput>,
    ) -> Result<PathBuf> {
        let report_path = self.output_dir.join("report.md");
        let mut body = String::new();
        body.push_str(&format!("# {} v{}\n\n", manifest.name, manifest.version));
        if !manifest.description.is_empty() {
            body.push_str(&format!("> {}\n\n", manifest.description));
        }
        body.push_str(&format!("Run completed at {}.\n\n", chrono::Utc::now().to_rfc3339()));
        for agent in &manifest.agents {
            body.push_str(&format!("## {}\n\n", agent.id));
            if let Some(out) = outputs.get(&agent.id) {
                body.push_str(&format!(
                    "*status: {:?} · exit: {:?}*\n\n",
                    out.status, out.exit_code
                ));
                if out.output_file.exists() {
                    let text = std::fs::read_to_string(&out.output_file).unwrap_or_default();
                    body.push_str(text.trim());
                    body.push_str("\n\n");
                }
            }
        }
        std::fs::write(&report_path, body)?;

        // Mirror the report into any readwrite mount that points at /output/.
        for mount in &manifest.mounts {
            if matches!(mount.mode, MountMode::Readwrite) {
                let host = paths::expand(&mount.host)?;
                let _ = std::fs::create_dir_all(&host);
                let _ = std::fs::copy(&report_path, host.join("report.md"));
            }
        }

        Ok(report_path)
    }

    /// Remove the run root. Only called for ephemeral sandboxes.
    pub fn tear_down(&self) -> Result<()> {
        if self.run_root.exists() {
            std::fs::remove_dir_all(&self.run_root)?;
        }
        Ok(())
    }

    pub fn pid_file(&self, agent_id: &str) -> PathBuf {
        self.state_dir.join(format!("{agent_id}.pid"))
    }
}

fn seed_from(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Err(anyhow!("install source missing: {}", src.display()));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        std::fs::remove_dir_all(dst).ok();
    }
    std::fs::create_dir_all(dst)?;
    let mut opts = CopyOptions::new();
    opts.copy_inside = true;
    opts.overwrite = true;
    copy_dir(src, dst, &opts)
        .with_context(|| format!("seeding {} → {}", src.display(), dst.display()))?;
    Ok(())
}

fn mirror_dir(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
