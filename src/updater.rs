//! GitHub Releases based self-update support.
//!
//! The release workflow publishes one archive per supported platform plus a
//! matching `.sha256` file. This module finds the current platform's archive,
//! verifies it, extracts the `dockagents` executable, and replaces the running
//! binary where the operating system allows it.

use std::cmp::Ordering;
use std::fs;
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use semver::{BuildMetadata, Version};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths;

pub const CURRENT_VERSION: &str = match option_env!("DOCKAGENTS_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub const DEFAULT_GITHUB_REPO: &str = "MrTigerST/dockagents";

const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UpdateConfig {
    #[serde(default = "default_check")]
    pub check: bool,
    #[serde(default)]
    pub auto_install: bool,
    #[serde(default = "default_github_repo")]
    pub github_repo: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            auto_install: false,
            github_repo: DEFAULT_GITHUB_REPO.to_string(),
        }
    }
}

impl UpdateConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_tag: String,
    pub release_url: String,
    pub asset_name: String,
    archive_url: String,
    sha256_url: String,
    archive_kind: ArchiveKind,
    binary_name: String,
}

#[derive(Debug, Clone)]
pub struct UpdateCheck {
    pub latest_tag: String,
    pub update: Option<UpdateInfo>,
}

#[derive(Debug, Clone)]
pub enum InstallResult {
    Replaced { path: PathBuf },
    Deferred { path: PathBuf },
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug)]
struct PlatformAsset {
    prefix: String,
    suffix: &'static str,
    binary_name: &'static str,
    archive_kind: ArchiveKind,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdateCheckCache {
    checked_at_unix: u64,
    latest_tag: String,
}

fn default_check() -> bool {
    true
}

fn default_github_repo() -> String {
    DEFAULT_GITHUB_REPO.to_string()
}

pub fn normalize_repo(repo: &str) -> Result<String> {
    let trimmed = repo.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .unwrap_or(trimmed);
    let mut parts = trimmed.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return Err(anyhow!(
            "GitHub repo must be `owner/repo` or a github.com URL, got '{repo}'"
        ));
    }
    Ok(format!("{owner}/{name}"))
}

pub fn maybe_notify_or_auto_update() {
    if env_truthy("DOCKAGENTS_NO_UPDATE_CHECK") {
        return;
    }

    let cfg = match crate::config::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::debug!("skipping update check; config could not be loaded: {e:#}");
            return;
        }
    };
    if !cfg.updates.check {
        return;
    }
    if !check_due().unwrap_or(false) {
        return;
    }

    let check = match check_latest(&cfg.updates.github_repo) {
        Ok(check) => check,
        Err(e) => {
            tracing::debug!("GitHub update check failed: {e:#}");
            return;
        }
    };
    let _ = remember_check(&check.latest_tag);

    let Some(info) = check.update else {
        return;
    };

    if cfg.updates.auto_install || env_truthy("DOCKAGENTS_AUTO_UPDATE") {
        eprintln!(
            "dockagents update available: {} -> {}. Installing from GitHub...",
            info.current_version, info.latest_tag
        );
        match install_release(&info) {
            Ok(InstallResult::Replaced { path }) => {
                eprintln!("dockagents updated at {}", path.display());
            }
            Ok(InstallResult::Deferred { path }) => {
                eprintln!(
                    "dockagents update downloaded; Windows will replace {} after this process exits.",
                    path.display()
                );
            }
            Err(e) => {
                eprintln!("dockagents automatic update failed: {e:#}");
            }
        }
    } else {
        eprintln!(
            "dockagents update available: {} -> {}. Run `dockagents update` to install from GitHub.",
            info.current_version, info.latest_tag
        );
    }
}

pub fn check_latest(repo: &str) -> Result<UpdateCheck> {
    let repo = normalize_repo(repo)?;
    let release = fetch_latest_release(&repo)?;
    let update = if is_newer_version(CURRENT_VERSION, &release.tag_name) {
        let platform = current_platform_asset()?;
        let asset = find_asset(&release.assets, &platform).with_context(|| {
            format!(
                "release {} has no asset matching {}*{}",
                release.tag_name, platform.prefix, platform.suffix
            )
        })?;
        let sha_asset = find_sha256_asset(&release.assets, &asset.name).with_context(|| {
            format!(
                "release {} has no sha256 file for {}",
                release.tag_name, asset.name
            )
        })?;
        Some(UpdateInfo {
            current_version: CURRENT_VERSION.to_string(),
            latest_tag: release.tag_name.clone(),
            release_url: release.html_url.clone(),
            asset_name: asset.name.clone(),
            archive_url: asset.browser_download_url.clone(),
            sha256_url: sha_asset.browser_download_url.clone(),
            archive_kind: platform.archive_kind,
            binary_name: platform.binary_name.to_string(),
        })
    } else {
        None
    };

    Ok(UpdateCheck {
        latest_tag: release.tag_name,
        update,
    })
}

pub fn install_release(info: &UpdateInfo) -> Result<InstallResult> {
    let archive = download_url(&info.archive_url)
        .with_context(|| format!("downloading {}", info.asset_name))?;
    let sha256 = download_url(&info.sha256_url)
        .with_context(|| format!("downloading {}.sha256", info.asset_name))?;
    verify_sha256(&archive, &sha256).with_context(|| format!("verifying {}", info.asset_name))?;

    let work_dir = update_work_dir(&info.latest_tag)?;
    let staged = work_dir.join(&info.binary_name);
    extract_binary(&archive, info.archive_kind, &info.binary_name, &staged)?;
    make_executable(&staged)?;

    apply_staged_executable(&staged)
}

fn fetch_latest_release(repo: &str) -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build();
    let resp = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", user_agent())
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    Ok(resp.into_json()?)
}

fn download_url(url: &str) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", user_agent())
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

fn user_agent() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
}

fn current_platform_asset() -> Result<PlatformAsset> {
    platform_asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_asset_for(os: &str, arch: &str) -> Result<PlatformAsset> {
    let (os_part, archive_kind, suffix, binary_name) = match os {
        "linux" => ("linux", ArchiveKind::TarGz, ".tar.gz", "dockagents"),
        "macos" => ("macos", ArchiveKind::TarGz, ".tar.gz", "dockagents"),
        "windows" => ("windows", ArchiveKind::Zip, ".zip", "dockagents.exe"),
        other => return Err(anyhow!("self-update is not supported on {other}")),
    };
    let arch_part = match arch {
        "x86_64" => "x86_64",
        "aarch64" if os == "macos" => "aarch64",
        other => {
            return Err(anyhow!(
                "self-update is not supported on {os}/{other}; install from GitHub manually"
            ))
        }
    };
    Ok(PlatformAsset {
        prefix: format!("dockagents-{os_part}-{arch_part}-"),
        suffix,
        binary_name,
        archive_kind,
    })
}

fn find_asset<'a>(assets: &'a [GitHubAsset], platform: &PlatformAsset) -> Option<&'a GitHubAsset> {
    assets.iter().find(|asset| {
        asset.name.starts_with(&platform.prefix) && asset.name.ends_with(platform.suffix)
    })
}

fn find_sha256_asset<'a>(assets: &'a [GitHubAsset], archive_name: &str) -> Option<&'a GitHubAsset> {
    let expected = format!("{archive_name}.sha256");
    assets.iter().find(|asset| asset.name == expected)
}

fn verify_sha256(bytes: &[u8], sha256_file: &[u8]) -> Result<()> {
    let expected_text = std::str::from_utf8(sha256_file).context("sha256 file is not UTF-8")?;
    let expected = expected_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("sha256 file is empty"))?;
    let actual = sha256_hex(bytes);
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(anyhow!(
            "sha256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

fn sha256_hex(buf: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(buf);
    hex::encode(h.finalize())
}

fn extract_binary(archive: &[u8], kind: ArchiveKind, binary_name: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
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

fn apply_staged_executable(staged: &Path) -> Result<InstallResult> {
    let current = std::env::current_exe().context("resolving current executable path")?;
    apply_staged_executable_to(staged, &current)
}

#[cfg(not(windows))]
fn apply_staged_executable_to(staged: &Path, current: &Path) -> Result<InstallResult> {
    let parent = current
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent: {}", current.display()))?;
    let file_name = current
        .file_name()
        .ok_or_else(|| anyhow!("current executable has no file name: {}", current.display()))?
        .to_string_lossy();
    let replacement = parent.join(format!(".{file_name}.update-{}", std::process::id()));
    fs::copy(staged, &replacement).with_context(|| {
        format!(
            "copying staged executable {} to {}",
            staged.display(),
            replacement.display()
        )
    })?;
    make_executable(&replacement)?;
    fs::rename(&replacement, current).with_context(|| {
        format!(
            "replacing current executable {} with {}",
            current.display(),
            replacement.display()
        )
    })?;
    Ok(InstallResult::Replaced {
        path: current.to_path_buf(),
    })
}

#[cfg(windows)]
fn apply_staged_executable_to(staged: &Path, current: &Path) -> Result<InstallResult> {
    let script = staged
        .parent()
        .ok_or_else(|| anyhow!("staged executable has no parent: {}", staged.display()))?
        .join("apply-update.ps1");
    fs::write(
        &script,
        r#"
param(
  [Parameter(Mandatory=$true)][int]$PidToWait,
  [Parameter(Mandatory=$true)][string]$Source,
  [Parameter(Mandatory=$true)][string]$Destination
)
$ErrorActionPreference = 'Stop'
try {
  Wait-Process -Id $PidToWait -ErrorAction SilentlyContinue
} catch {}
Copy-Item -LiteralPath $Source -Destination $Destination -Force
Remove-Item -LiteralPath $Source -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $MyInvocation.MyCommand.Path -Force -ErrorAction SilentlyContinue
"#,
    )
    .with_context(|| format!("writing update helper {}", script.display()))?;

    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script)
        .arg("-PidToWait")
        .arg(std::process::id().to_string())
        .arg("-Source")
        .arg(staged)
        .arg("-Destination")
        .arg(current)
        .spawn()
        .with_context(|| format!("starting update helper {}", script.display()))?;

    Ok(InstallResult::Deferred {
        path: current.to_path_buf(),
    })
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

fn update_work_dir(tag: &str) -> Result<PathBuf> {
    let safe_tag = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let dir = paths::state_dir()?
        .join("self-update")
        .join(format!("{safe_tag}-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cache_path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("update-check.json"))
}

fn check_due() -> Result<bool> {
    let path = cache_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return Ok(true),
    };
    let cache: UpdateCheckCache = match serde_json::from_str(&raw) {
        Ok(cache) => cache,
        Err(_) => return Ok(true),
    };
    Ok(now_unix().saturating_sub(cache.checked_at_unix) >= CHECK_INTERVAL.as_secs())
}

fn remember_check(latest_tag: &str) -> Result<()> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let cache = UpdateCheckCache {
        checked_at_unix: now_unix(),
        latest_tag: latest_tag.to_string(),
    };
    fs::write(path, serde_json::to_vec_pretty(&cache)?)?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            !value.is_empty() && !matches!(value.as_str(), "0" | "false" | "no" | "off")
        })
        .unwrap_or(false)
}

fn is_newer_version(current: &str, latest: &str) -> bool {
    let Ok(current_version) = parse_version(current) else {
        return normalize_tag(current) != normalize_tag(latest);
    };
    let Ok(latest_version) = parse_version(latest) else {
        return normalize_tag(current) != normalize_tag(latest);
    };

    match version_without_build(&latest_version).cmp(&version_without_build(&current_version)) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match (build_run(&latest_version), build_run(&current_version)) {
            (Some(latest_run), Some(current_run)) => latest_run > current_run,
            (Some(_), None) => normalize_tag(current) != normalize_tag(latest),
            _ => false,
        },
    }
}

fn parse_version(raw: &str) -> Result<Version, semver::Error> {
    Version::parse(normalize_tag(raw))
}

fn normalize_tag(raw: &str) -> &str {
    raw.trim().strip_prefix('v').unwrap_or_else(|| raw.trim())
}

fn version_without_build(version: &Version) -> Version {
    let mut version = version.clone();
    version.build = BuildMetadata::EMPTY;
    version
}

fn build_run(version: &Version) -> Option<u64> {
    let mut parts = version.build.as_str().split('.');
    if parts.next()? != "build" {
        return None;
    }
    parts.next()?.parse().ok()
}

pub fn prompt_install(info: &UpdateInfo, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "an update is available, but stdin is not interactive; rerun with `dockagents update --yes`"
        ));
    }
    print!(
        "Install dockagents {} from GitHub now? [y/N] ",
        info.latest_tag
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_metadata_run_number_makes_release_newer() {
        assert!(is_newer_version(
            "0.1.0+build.41.abcdef0",
            "v0.1.0+build.42.1234567"
        ));
    }

    #[test]
    fn same_build_is_not_newer() {
        assert!(!is_newer_version(
            "0.1.0+build.42.abcdef0",
            "v0.1.0+build.42.1234567"
        ));
    }

    #[test]
    fn release_asset_pattern_matches_supported_platforms() {
        let linux = platform_asset_for("linux", "x86_64").unwrap();
        assert_eq!(linux.prefix, "dockagents-linux-x86_64-");
        assert_eq!(linux.suffix, ".tar.gz");

        let windows = platform_asset_for("windows", "x86_64").unwrap();
        assert_eq!(windows.prefix, "dockagents-windows-x86_64-");
        assert_eq!(windows.suffix, ".zip");

        let mac = platform_asset_for("macos", "aarch64").unwrap();
        assert_eq!(mac.prefix, "dockagents-macos-aarch64-");
        assert_eq!(mac.suffix, ".tar.gz");
    }
}
