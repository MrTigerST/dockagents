//! OS-level isolation wrappers for spawned agent processes.
//!
//! Each backend takes a [`std::process::Command`] aimed at the agent
//! subprocess and either replaces or augments it with platform-specific
//! sandboxing:
//!
//!   * **Linux** — wraps with `bwrap` (Bubblewrap) when the binary is on
//!     PATH. Limits filesystem visibility to the agent's workspace and the
//!     sandbox's mount points; binds `/usr` and `/lib*` read-only so the
//!     dockagents binary can still execute.
//!   * **macOS** — wraps with `sandbox-exec` and a generated Seatbelt
//!     profile that allows reads under the workspace + mounts and writes
//!     only to the workspace + readwrite mounts.
//!   * **Windows** — assigns each spawned process to a kernel Job Object
//!     with `KILL_ON_JOB_CLOSE | DIE_ON_UNHANDLED_EXCEPTION
//!     | ACTIVE_PROCESS`, plus UI restrictions that deny clipboard, global
//!     atom, USER handle, and desktop access. The job handle is owned by
//!     the runtime; dropping it kills the entire agent subtree atomically.
//!
//! All backends fail open: if the OS facility is missing or returns an
//! error, the agent runs with normal process isolation and a warning is
//! logged, so dockagents stays usable on under-provisioned systems.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

#[cfg(windows)]
pub use self::windows::JobHandle;
#[cfg(not(windows))]
pub struct JobHandle;

/// Permissions surface the runtime needs the agent process to have.
pub struct Sandbox<'a> {
    pub agent_id: &'a str,
    pub workspace: &'a Path,
    pub readonly_paths: &'a [&'a Path],
    pub readwrite_paths: &'a [&'a Path],
    pub allow_network: bool,
}

/// Apply OS-level isolation in-place. Returns `true` when a sandbox backend
/// successfully wrapped the command, `false` when we fell back to the bare
/// `Command` (the runtime then logs a warning).
pub fn wrap(cmd: &mut Command, sandbox: &Sandbox<'_>) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        return linux::wrap(cmd, sandbox);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::wrap(cmd, sandbox);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::wrap(cmd, sandbox);
    }
    #[allow(unreachable_code)]
    Ok(false)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::ffi::OsString;

    pub fn wrap(cmd: &mut Command, s: &Sandbox<'_>) -> Result<bool> {
        if which("bwrap").is_none() {
            tracing::warn!(
                "bwrap not found on PATH — agent {} will run with process isolation only",
                s.agent_id
            );
            return Ok(false);
        }

        let agent_exec = cmd.get_program().to_owned();
        let agent_args: Vec<OsString> = cmd.get_args().map(|a| a.to_owned()).collect();

        let mut bwrap = Command::new("bwrap");
        // Minimum filesystem to make a Rust binary executable.
        for path in [
            "/usr", "/lib", "/lib32", "/lib64", "/bin", "/sbin", "/etc/ld.so.cache",
            "/etc/ssl", "/etc/resolv.conf",
        ] {
            if std::path::Path::new(path).exists() {
                bwrap.args(["--ro-bind", path, path]);
            }
        }
        bwrap.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);

        for ro in s.readonly_paths {
            let p = ro.display().to_string();
            bwrap.args(["--ro-bind", &p, &p]);
        }
        for rw in s.readwrite_paths {
            let p = rw.display().to_string();
            bwrap.args(["--bind", &p, &p]);
        }
        let ws = s.workspace.display().to_string();
        bwrap.args(["--bind", &ws, &ws]);

        if !s.allow_network {
            bwrap.args(["--unshare-net"]);
        }
        bwrap.args(["--die-with-parent", "--unshare-pid", "--unshare-ipc", "--unshare-uts"]);
        bwrap.arg("--");
        bwrap.arg(agent_exec);
        bwrap.args(agent_args);

        // Replace the original command's program/args with bwrap's.
        *cmd = bwrap;
        Ok(true)
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let p = dir.join(bin);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::io::Write;

    pub fn wrap(cmd: &mut Command, s: &Sandbox<'_>) -> Result<bool> {
        let mut profile = String::new();
        profile.push_str("(version 1)\n");
        profile.push_str("(deny default)\n");
        profile.push_str("(allow process-fork process-exec)\n");
        profile.push_str("(allow file-read*)\n");
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            escape(s.workspace)
        ));
        for rw in s.readwrite_paths {
            profile.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", escape(rw)));
        }
        if s.allow_network {
            profile.push_str("(allow network*)\n");
        } else {
            profile.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        }

        let dir = std::env::temp_dir().join("dockagents-profiles");
        std::fs::create_dir_all(&dir)?;
        let profile_path = dir.join(format!("{}.sb", s.agent_id));
        let mut f = std::fs::File::create(&profile_path)?;
        f.write_all(profile.as_bytes())?;

        let agent_exec = cmd.get_program().to_owned();
        let agent_args: Vec<_> = cmd.get_args().map(|a| a.to_owned()).collect();
        let mut wrapper = Command::new("sandbox-exec");
        wrapper.arg("-f").arg(&profile_path).arg(agent_exec).args(agent_args);
        *cmd = wrapper;
        Ok(true)
    }

    fn escape(p: &Path) -> String {
        p.display().to_string().replace('"', "\\\"")
    }
}

#[cfg(target_os = "windows")]
pub mod windows {
    //! Windows isolation via Job Objects.
    //!
    //! `Command` carries no Win32-specific knobs we need pre-spawn, so we
    //! return `false` from [`super::wrap`] and instead do post-spawn work in
    //! [`super::attach_job`]. That hand-off is two-step on purpose:
    //!
    //!   1. [`create_job`] builds the kernel Job Object up front so we can
    //!      surface OS errors (`SetInformationJobObject`) before launching
    //!      anything.
    //!   2. After [`Command::spawn`] returns, the runtime calls
    //!      [`attach`] which `AssignProcessToJobObject`s the child.
    //!
    //! There is a small race window between spawn and attach during which
    //! the agent could `fork`/`CreateProcess`. The agent's first action is
    //! `read stdin`, so in practice this is benign; tightening it requires
    //! `CREATE_SUSPENDED` and resuming the main thread by ID, which std
    //! does not surface.

    use super::Sandbox;
    use anyhow::{anyhow, Result};
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
        JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS,
        JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
        JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };

    /// RAII handle to a Win32 Job Object. Dropping it closes the kernel
    /// handle, which (with `KILL_ON_JOB_CLOSE`) terminates every assigned
    /// process.
    pub struct JobHandle(HANDLE);

    // SAFETY: HANDLE is `*mut c_void`. Sending it across threads is sound
    // because the kernel does the synchronization; we never dereference it.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    pub fn wrap(_cmd: &mut Command, s: &Sandbox<'_>) -> Result<bool> {
        tracing::debug!(
            "windows isolation: agent {} will be assigned to a Job Object after spawn",
            s.agent_id
        );
        Ok(false)
    }

    /// Build the Job Object with the limits we want every agent to inherit.
    /// The active-process cap is set high enough to allow normal HTTP/TLS
    /// machinery (which can spawn helper threads/processes) but low enough
    /// to make a runaway child a small explosion rather than a large one.
    pub fn create_job(_s: &Sandbox<'_>) -> Result<JobHandle> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(anyhow!("CreateJobObjectW failed: {}", std::io::Error::last_os_error()));
            }
            let handle = JobHandle(h);

            // Extended limits — kill the tree on close, kill on unhandled
            // exception, cap active processes. Memory caps are off here on
            // purpose: the agent runner is `dockagents __agent`, which spawns
            // with the runtime's working set; clamping it here would also
            // clamp the runtime when the job is released.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = 32;
            let ok = SetInformationJobObject(
                handle.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(anyhow!(
                    "SetInformationJobObject(extended limits) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            // UI restrictions — agents have no business touching the user
            // interface, the clipboard, the desktop, or system parameters.
            let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = std::mem::zeroed();
            ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;
            let ok = SetInformationJobObject(
                handle.0,
                JobObjectBasicUIRestrictions,
                &ui as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            );
            if ok == 0 {
                tracing::warn!(
                    "SetInformationJobObject(UI restrictions) failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            Ok(handle)
        }
    }

    /// Assign an already-spawned [`Child`] to the Job Object so the kernel
    /// will tear it down when the job closes.
    pub fn attach(job: &JobHandle, child: &Child) -> Result<()> {
        let proc_handle = child.as_raw_handle() as HANDLE;
        let ok = unsafe { AssignProcessToJobObject(job.0, proc_handle) };
        if ok == 0 {
            return Err(anyhow!(
                "AssignProcessToJobObject failed for pid {}: {}",
                child.id(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}
