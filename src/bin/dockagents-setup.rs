use std::fs;
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

const REPO: &str = "MrTigerST/dockagents";
const VERSION: &str = match option_env!("DOCKAGENTS_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Debug, Parser)]
#[command(
    name = "dockagents-setup",
    version = VERSION,
    about = "Install (or uninstall) DockAgents and optionally manage PATH"
)]
struct Args {
    /// Directory where dockagents should be installed. Also the directory
    /// `--uninstall` looks in.
    #[arg(long)]
    install_dir: Option<PathBuf>,
    /// Add the install directory to the user's PATH.
    #[arg(long, conflicts_with = "no_add_to_path")]
    add_to_path: bool,
    /// Do not add the install directory to PATH.
    #[arg(long)]
    no_add_to_path: bool,
    /// Use defaults without prompting.
    #[arg(long)]
    yes: bool,
    /// Remove DockAgents instead of installing. Deletes the binary, scrubs
    /// the PATH entry that was previously added, and (with --purge) wipes
    /// `~/.dockagents/` user data.
    #[arg(long, conflicts_with_all = ["add_to_path", "no_add_to_path"])]
    uninstall: bool,
    /// With --uninstall, also delete `~/.dockagents/` (config, keys, installed sandboxes).
    #[arg(long, requires = "uninstall")]
    purge: bool,
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    TarGz,
    Zip,
}

struct PlatformRelease {
    archive_name: String,
    archive_kind: ArchiveKind,
    binary_name: &'static str,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("setup failed: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let interactive = io::stdin().is_terminal() && !args.yes;
    let install_dir = match args.install_dir {
        Some(path) => path,
        None if interactive && !args.uninstall => prompt_install_dir(&default_install_dir()?)?,
        None => default_install_dir()?,
    };

    if args.uninstall {
        return uninstall(&install_dir, args.purge, interactive);
    }

    let add_to_path = if args.add_to_path {
        true
    } else if args.no_add_to_path {
        false
    } else if interactive {
        prompt_yes_no(
            &format!("Add {} to your PATH?", install_dir.display()),
            true,
        )?
    } else {
        true
    };

    let release = platform_release()?;
    let source = find_or_download_binary(&release)?;
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let dest = install_dir.join(release.binary_name);
    fs::copy(&source, &dest)
        .with_context(|| format!("copying {} to {}", source.display(), dest.display()))?;
    make_executable(&dest)?;

    if add_to_path {
        add_install_dir_to_path(&install_dir)?;
    }

    // On Windows, copy ourselves alongside the dockagents binary and register
    // an Apps & features uninstall entry so the user can remove dockagents
    // from Settings → Apps the way they would any other application.
    #[cfg(windows)]
    {
        let setup_copy = install_dir.join("dockagents-setup.exe");
        if let Ok(self_exe) = std::env::current_exe() {
            // Best-effort — if we're already running from the install dir, skip.
            if self_exe != setup_copy {
                let _ = fs::copy(&self_exe, &setup_copy);
            }
        }
        let _ = register_windows_uninstaller(&install_dir, &setup_copy);
    }

    println!("Installed dockagents {} to {}", VERSION, dest.display());
    if add_to_path {
        println!("PATH updated. Restart your shell before running `dockagents`.");
    } else {
        println!(
            "PATH unchanged. Run dockagents with `{}` or add `{}` to PATH later.",
            dest.display(),
            install_dir.display()
        );
    }
    Ok(())
}

fn default_install_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(local).join("dockagents"));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(home.join(".local").join("bin"))
}

fn prompt_install_dir(default: &Path) -> Result<PathBuf> {
    print!("Install dockagents to [{}]: ", default.display());
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.is_empty() {
        Ok(default.to_path_buf())
    } else {
        Ok(PathBuf::from(answer))
    }
}

fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool> {
    let marker = if default { "Y/n" } else { "y/N" };
    print!("{prompt} [{marker}] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn platform_release() -> Result<PlatformRelease> {
    let (platform, kind, suffix, binary_name) = match std::env::consts::OS {
        "linux" => ("linux-x86_64", ArchiveKind::TarGz, "tar.gz", "dockagents"),
        "windows" => ("windows-x86_64", ArchiveKind::Zip, "zip", "dockagents.exe"),
        "macos" => {
            let arch = match std::env::consts::ARCH {
                "x86_64" => "x86_64",
                "aarch64" => "aarch64",
                other => return Err(anyhow!("unsupported macOS architecture: {other}")),
            };
            return Ok(PlatformRelease {
                archive_name: format!("dockagents-macos-{arch}-{VERSION}.tar.gz"),
                archive_kind: ArchiveKind::TarGz,
                binary_name: "dockagents",
            });
        }
        other => return Err(anyhow!("unsupported operating system: {other}")),
    };

    if std::env::consts::ARCH != "x86_64" {
        return Err(anyhow!(
            "unsupported architecture for {}: {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }

    Ok(PlatformRelease {
        archive_name: format!("dockagents-{platform}-{VERSION}.{suffix}"),
        archive_kind: kind,
        binary_name,
    })
}

fn find_or_download_binary(release: &PlatformRelease) -> Result<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.join(release.binary_name)))
        .filter(|path| path.exists());
    if let Some(path) = sibling {
        return Ok(path);
    }

    println!(
        "Downloading {} from GitHub Releases...",
        release.archive_name
    );
    let archive_url = format!(
        "https://github.com/{REPO}/releases/download/v{VERSION}/{}",
        release.archive_name
    );
    let sha_url = format!("{archive_url}.sha256");
    let archive = download(&archive_url)?;
    let sha = download(&sha_url)?;
    verify_sha256(&archive, &sha)?;

    let dir = std::env::temp_dir().join(format!("dockagents-setup-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).ok();
    }
    fs::create_dir_all(&dir)?;
    let staged = dir.join(release.binary_name);
    extract_binary(&archive, release.archive_kind, release.binary_name, &staged)?;
    make_executable(&staged)?;
    Ok(staged)
}

fn download(url: &str) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", concat!(env!("CARGO_PKG_NAME"), "-setup"))
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn verify_sha256(bytes: &[u8], sha_file: &[u8]) -> Result<()> {
    let expected_text = std::str::from_utf8(sha_file).context("sha256 file is not UTF-8")?;
    let expected = expected_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("sha256 file is empty"))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex::encode(hasher.finalize());
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(anyhow!(
            "sha256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

fn extract_binary(archive: &[u8], kind: ArchiveKind, binary_name: &str, dest: &Path) -> Result<()> {
    match kind {
        ArchiveKind::TarGz => extract_binary_from_targz(archive, binary_name, dest),
        ArchiveKind::Zip => extract_binary_from_zip(archive, binary_name, dest),
    }
}

fn extract_binary_from_targz(archive: &[u8], binary_name: &str, dest: &Path) -> Result<()> {
    let gz = GzDecoder::new(Cursor::new(archive));
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.file_name().and_then(|name| name.to_str()) != Some(binary_name) {
            continue;
        }
        let mut out = fs::File::create(dest)?;
        io::copy(&mut entry, &mut out)?;
        return Ok(());
    }
    Err(anyhow!("archive did not contain {binary_name}"))
}

fn extract_binary_from_zip(archive: &[u8], binary_name: &str, dest: &Path) -> Result<()> {
    let cursor = Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if !file.is_file() {
            continue;
        }
        let name = file
            .enclosed_name()
            .and_then(|path| path.file_name().map(|name| name.to_owned()));
        if name.as_deref().and_then(|name| name.to_str()) != Some(binary_name) {
            continue;
        }
        let mut out = fs::File::create(dest)?;
        io::copy(&mut file, &mut out)?;
        return Ok(());
    }
    Err(anyhow!("archive did not contain {binary_name}"))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn add_install_dir_to_path(dir: &Path) -> Result<()> {
    if path_contains(dir) {
        println!("{} is already on PATH.", dir.display());
        return Ok(());
    }

    #[cfg(windows)]
    {
        add_to_windows_user_path(dir)
    }
    #[cfg(not(windows))]
    {
        add_to_unix_profile(dir)
    }
}

fn path_contains(dir: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|entry| same_path(&entry, dir))
}

fn same_path(a: &Path, b: &Path) -> bool {
    let a = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

#[cfg(windows)]
fn add_to_windows_user_path(dir: &Path) -> Result<()> {
    let dir = dir.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$dir = '{dir}'; \
         $path = [Environment]::GetEnvironmentVariable('Path','User'); \
         if ([string]::IsNullOrWhiteSpace($path)) {{ $new = $dir }} \
         elseif (($path -split ';') -notcontains $dir) {{ $new = $dir + ';' + $path }} \
         else {{ $new = $path }}; \
         [Environment]::SetEnvironmentVariable('Path', $new, 'User')"
    );
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .status()
        .context("updating user PATH through PowerShell")?;
    if !status.success() {
        return Err(anyhow!("PowerShell failed while updating PATH"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn add_to_unix_profile(dir: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    let profile = if cfg!(target_os = "macos") {
        home.join(".zprofile")
    } else {
        home.join(".profile")
    };
    let path_expr = shell_path_expr(dir, &home);
    let block = format!(
        "\n# DockAgents\ncase \":$PATH:\" in\n  *:\"{path_expr}\":*) ;;\n  *) export PATH=\"{path_expr}:$PATH\" ;;\nesac\n"
    );
    let existing = fs::read_to_string(&profile).unwrap_or_default();
    if !existing.contains("# DockAgents") || !existing.contains(&path_expr) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&profile)
            .with_context(|| format!("opening {}", profile.display()))?;
        file.write_all(block.as_bytes())?;
    }
    println!("Updated {}", profile.display());
    Ok(())
}

#[cfg(not(windows))]
fn shell_path_expr(dir: &Path, home: &Path) -> String {
    if let Ok(rel) = dir.strip_prefix(home) {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            "$HOME".to_string()
        } else {
            format!("$HOME/{rel}")
        }
    } else {
        dir.to_string_lossy().replace('"', "\\\"")
    }
}

// ── uninstall ──────────────────────────────────────────────────────────────

fn uninstall(install_dir: &Path, purge: bool, interactive: bool) -> Result<()> {
    let binary = install_dir.join(if cfg!(windows) {
        "dockagents.exe"
    } else {
        "dockagents"
    });

    if interactive {
        println!("This will remove:");
        println!("  binary:  {}", binary.display());
        println!("  PATH entry for: {}", install_dir.display());
        if purge {
            if let Some(home) = dirs::home_dir() {
                println!("  user data:  {}", home.join(".dockagents").display());
            }
        }
        let ok = prompt_yes_no("Continue?", false)?;
        if !ok {
            println!("Aborted.");
            return Ok(());
        }
    }

    // 1. Delete the binary.
    if binary.exists() {
        fs::remove_file(&binary)
            .with_context(|| format!("removing {}", binary.display()))?;
        println!("Removed {}", binary.display());
    } else {
        println!("(no binary at {} — already gone?)", binary.display());
    }

    // 2. Try to remove the install_dir itself if empty.
    if install_dir.exists() {
        let empty = fs::read_dir(install_dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if empty {
            let _ = fs::remove_dir(install_dir);
        }
    }

    // 3. Scrub the PATH entry we added (best-effort).
    remove_install_dir_from_path(install_dir)?;

    // 4a. On Windows, drop the Apps & features registry entry so we don't
    // leave a stale "Uninstall" row pointing at a deleted exe.
    #[cfg(windows)]
    {
        let _ = unregister_windows_uninstaller();
        // The setup copy alongside dockagents.exe is what's running this
        // uninstall; we can't delete ourselves on Windows mid-execution.
        // Schedule a delete on next reboot via MOVEFILEEX_DELAY_UNTIL_REBOOT
        // would work, but for now we just leave it — the install_dir is
        // already removed if it was empty, and orphan setup.exe in a
        // non-empty dir is harmless.
        let setup_copy = install_dir.join("dockagents-setup.exe");
        if setup_copy.exists() && setup_copy != std::env::current_exe().unwrap_or_default() {
            let _ = fs::remove_file(&setup_copy);
        }
    }

    // 4. With --purge, wipe user state.
    if purge {
        if let Some(home) = dirs::home_dir() {
            let data = home.join(".dockagents");
            if data.exists() {
                fs::remove_dir_all(&data)
                    .with_context(|| format!("removing {}", data.display()))?;
                println!("Removed {}", data.display());
            }
        }
    }

    println!("Uninstall complete.");
    if !purge {
        println!(
            "Note: `~/.dockagents/` (config, keys, installed sandboxes) was kept. \
             Re-run with --purge to delete it too."
        );
    }
    Ok(())
}

fn remove_install_dir_from_path(dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        remove_from_windows_user_path(dir)
    }
    #[cfg(not(windows))]
    {
        remove_from_unix_profile(dir)
    }
}

#[cfg(windows)]
fn remove_from_windows_user_path(dir: &Path) -> Result<()> {
    let dir = dir.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$dir = '{dir}'; \
         $path = [Environment]::GetEnvironmentVariable('Path','User'); \
         if ($null -eq $path) {{ return }}; \
         $new = (($path -split ';') | Where-Object {{ $_ -and ($_ -ne $dir) }}) -join ';'; \
         [Environment]::SetEnvironmentVariable('Path', $new, 'User')"
    );
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .status()
        .context("updating user PATH through PowerShell")?;
    if !status.success() {
        eprintln!("warning: could not scrub PATH (continuing)");
    }
    Ok(())
}

#[cfg(windows)]
fn register_windows_uninstaller(install_dir: &Path, setup_exe: &Path) -> Result<()> {
    let install = install_dir.to_string_lossy().replace('\'', "''");
    let setup = setup_exe.to_string_lossy().replace('\'', "''");
    let version = VERSION.replace('\'', "''");
    let script = format!(
        "$key = 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DockAgents'; \
         New-Item -Path $key -Force | Out-Null; \
         Set-ItemProperty -Path $key -Name 'DisplayName'    -Value 'DockAgents'; \
         Set-ItemProperty -Path $key -Name 'DisplayVersion' -Value '{version}'; \
         Set-ItemProperty -Path $key -Name 'Publisher'      -Value 'DockAgents'; \
         Set-ItemProperty -Path $key -Name 'InstallLocation' -Value '{install}'; \
         Set-ItemProperty -Path $key -Name 'DisplayIcon'    -Value '{setup}'; \
         Set-ItemProperty -Path $key -Name 'UninstallString' -Value ('\"' + '{setup}' + '\" --uninstall --yes'); \
         Set-ItemProperty -Path $key -Name 'QuietUninstallString' -Value ('\"' + '{setup}' + '\" --uninstall --yes'); \
         Set-ItemProperty -Path $key -Name 'URLInfoAbout'   -Value 'https://dockagents.net'; \
         Set-ItemProperty -Path $key -Name 'HelpLink'       -Value 'https://dockagents.net/docs'; \
         Set-ItemProperty -Path $key -Name 'NoModify' -Value 1 -Type DWord; \
         Set-ItemProperty -Path $key -Name 'NoRepair' -Value 1 -Type DWord"
    );
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .status()
        .context("registering Windows uninstaller via PowerShell")?;
    if !status.success() {
        eprintln!("warning: could not register Apps & features entry");
    } else {
        println!("Registered DockAgents in Settings → Apps & features.");
    }
    Ok(())
}

#[cfg(windows)]
fn unregister_windows_uninstaller() -> Result<()> {
    let script = "$key = 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DockAgents'; \
                  if (Test-Path $key) { Remove-Item -Path $key -Recurse -Force }";
    let _ = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .status();
    Ok(())
}

#[cfg(not(windows))]
fn remove_from_unix_profile(dir: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    let profile = if cfg!(target_os = "macos") {
        home.join(".zprofile")
    } else {
        home.join(".profile")
    };
    if !profile.exists() {
        return Ok(());
    }
    let path_expr = shell_path_expr(dir, &home);
    let raw = fs::read_to_string(&profile).unwrap_or_default();
    if !raw.contains("# DockAgents") {
        return Ok(());
    }

    // Walk the file and drop the DockAgents block. The install code writes:
    //   \n# DockAgents\n case ":$PATH:" in ... esac\n
    // We delete from the "# DockAgents" line through the matching `esac` line
    // (inclusive), but only if the block contains our path_expr.
    let mut out = String::with_capacity(raw.len());
    let mut lines = raw.lines().peekable();
    let mut dropped = false;
    while let Some(line) = lines.next() {
        if line.trim() == "# DockAgents" {
            // collect block until esac
            let mut block = String::from(line);
            block.push('\n');
            let mut found_path = false;
            while let Some(next) = lines.next() {
                block.push_str(next);
                block.push('\n');
                if next.contains(&path_expr) {
                    found_path = true;
                }
                if next.trim() == "esac" {
                    break;
                }
            }
            if found_path {
                dropped = true;
                continue;
            } else {
                out.push_str(&block);
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if dropped {
        fs::write(&profile, out.trim_end_matches('\n').to_string() + "\n")
            .with_context(|| format!("writing {}", profile.display()))?;
        println!("Cleaned PATH entry from {}", profile.display());
    }
    Ok(())
}
