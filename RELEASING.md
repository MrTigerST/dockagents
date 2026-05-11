# Releasing & installing dockagents

This document covers three things:

1. **Producing release binaries** — how the GitHub Actions workflow turns a
   commit into setup executables and portable binaries for Windows, Linux,
   and macOS.
2. **Installing dockagents** from those setup executables or portable archives
   on each OS.
3. **Updating dockagents** from GitHub Releases after installation.

## How releases are produced

A GitHub Actions workflow ([`.github/workflows/release.yml`](.github/workflows/release.yml))
builds and publishes a release on every push to `main` that touches the
Rust source tree (`src/**`, `Cargo.toml`, `Cargo.lock`, or the workflow
itself). It can also be triggered manually from the **Actions** tab via
**Run workflow**.

### Auto-versioning

Versions are computed at build time — there is no manual tagging step. The
format is:

```
v<cargo_version>+build.<run_number>.<short_sha>
```

For example: `v0.1.0+build.42.a1b2c3d`.

* `cargo_version` is read from `Cargo.toml`.
* `run_number` is the GitHub Actions run number for that workflow.
* `short_sha` is the first 7 characters of the commit SHA.

Bumping the version is therefore as simple as editing the `version` field
in `Cargo.toml` and pushing.

The workflow also embeds the full generated build version in the executable
as `DOCKAGENTS_BUILD_VERSION`, so `dockagents update` can compare the current
binary against GitHub release tags that include the build number.

### Build matrix

| Platform | Runner       | Target triple                | Archive |
|----------|--------------|------------------------------|---------|
| Linux    | ubuntu-latest| `x86_64-unknown-linux-gnu`   | `.tar.gz` |
| Windows  | windows-latest | `x86_64-pc-windows-msvc`   | `.zip`  |
| macOS x86_64 | macos-13 | `x86_64-apple-darwin`        | `.tar.gz` |
| macOS arm64 | macos-14  | `aarch64-apple-darwin`       | `.tar.gz` |

Each archive contains the `dockagents` binary plus `README.md`, this file,
and `LICENSE` if present. A matching `.sha256` is uploaded alongside every
archive so installers can verify the download.

Each build also publishes a standalone `dockagents-setup-*` executable for
that operating system. The setup executable installs the matching DockAgents
binary and prompts the user to choose whether the install directory should be
added to that operating system's `PATH`.

DockAgents uses those same release assets for self-updates: it selects the
archive for the current operating system, verifies the matching `.sha256`,
extracts the executable, and installs it in place. Windows replacement is
scheduled after the running `dockagents.exe` process exits.

### What the release looks like

The job produces a GitHub Release tagged with the auto-version and attaches
all four archives plus their SHA-256 sums. Release notes list the cargo
version, build number, and a download table.

## Installing dockagents from a release

Grab the latest setup executable for your platform from the
[**Releases**](https://github.com/MrTigerST/dockagents/releases) page.

Run it directly:

```bash
./dockagents-setup-linux-x86_64-0.1.0+build.42.a1b2c3d
```

```powershell
.\dockagents-setup-windows-x86_64-0.1.0+build.42.a1b2c3d.exe
```

The setup executable asks where to install `dockagents` and whether to add
that install directory to `PATH`. Use `--yes` to accept defaults,
`--install-dir <path>` to choose a directory non-interactively,
`--add-to-path` to force PATH setup, or `--no-add-to-path` to skip it.

Portable archives are still available for manual installs:

### Linux (x86_64)

```bash
VERSION=v0.1.0+build.42.a1b2c3d   # paste the actual tag
curl -L -o dockagents.tar.gz \
  "https://github.com/MrTigerST/dockagents/releases/download/${VERSION}/dockagents-linux-x86_64-${VERSION#v}.tar.gz"

# Verify
curl -L -o dockagents.tar.gz.sha256 \
  "https://github.com/MrTigerST/dockagents/releases/download/${VERSION}/dockagents-linux-x86_64-${VERSION#v}.tar.gz.sha256"
sha256sum -c dockagents.tar.gz.sha256

# Install
tar -xzf dockagents.tar.gz
sudo install -m 0755 dockagents /usr/local/bin/dockagents
dockagents --version
```

### macOS (Apple Silicon and Intel)

Apple Silicon Macs use the `aarch64` archive; Intel Macs use the `x86_64`
archive.

```bash
ARCH=$(uname -m)            # arm64 → aarch64; x86_64 stays the same
case "$ARCH" in
  arm64) ARCH=aarch64 ;;
esac

VERSION=v0.1.0+build.42.a1b2c3d   # paste the actual tag
curl -L -o dockagents.tar.gz \
  "https://github.com/MrTigerST/dockagents/releases/download/${VERSION}/dockagents-macos-${ARCH}-${VERSION#v}.tar.gz"

# Verify
curl -L -o dockagents.tar.gz.sha256 \
  "https://github.com/MrTigerST/dockagents/releases/download/${VERSION}/dockagents-macos-${ARCH}-${VERSION#v}.tar.gz.sha256"
shasum -a 256 -c dockagents.tar.gz.sha256

# Install
tar -xzf dockagents.tar.gz
sudo install -m 0755 dockagents /usr/local/bin/dockagents
dockagents --version
```

The binary is unsigned. The first time you run it, macOS Gatekeeper will
quarantine it. Either right-click → **Open** in Finder, or remove the
quarantine attribute:

```bash
xattr -d com.apple.quarantine /usr/local/bin/dockagents
```

### Windows (x86_64)

In PowerShell:

```powershell
$Version = 'v0.1.0+build.42.a1b2c3d'   # paste the actual tag
$Url     = "https://github.com/MrTigerST/dockagents/releases/download/$Version/dockagents-windows-x86_64-$($Version.TrimStart('v')).zip"

Invoke-WebRequest $Url -OutFile dockagents.zip

# Verify
Invoke-WebRequest "$Url.sha256" -OutFile dockagents.zip.sha256
$expected = (Get-Content dockagents.zip.sha256).Split(' ')[0]
$actual   = (Get-FileHash dockagents.zip -Algorithm SHA256).Hash.ToLower()
if ($expected -ne $actual) { throw "SHA256 mismatch" }

# Install — pick a directory on your PATH
Expand-Archive dockagents.zip -DestinationPath "$env:LOCALAPPDATA\dockagents" -Force

# Add to PATH for the current user (one-time)
[Environment]::SetEnvironmentVariable(
  "PATH",
  "$env:LOCALAPPDATA\dockagents;" + [Environment]::GetEnvironmentVariable("PATH","User"),
  "User"
)

# Restart the shell, then:
dockagents --version
```

Windows SmartScreen may warn the first time you run an unsigned binary —
**More info → Run anyway**.

## Updating dockagents from GitHub

The CLI checks GitHub Releases once per day and prints a notice if a newer
OS-specific executable is available. To check manually:

```bash
dockagents update --check
```

To download, verify, and install the latest release executable:

```bash
dockagents update --yes
```

Automatic installs are opt-in:

```bash
dockagents config set-updates --auto-install true
```

Disable the daily notice with `DOCKAGENTS_NO_UPDATE_CHECK=1` for one shell,
or globally with:

```bash
dockagents config set-updates --check false
```

## Verifying a release artifact

Each archive ships with a `<archive>.sha256` file containing the lower-case
hex digest. Compare it with what you downloaded:

```bash
# Linux / macOS
sha256sum -c dockagents-linux-x86_64-0.1.0+build.42.a1b2c3d.tar.gz.sha256
```

```powershell
# Windows
$expected = (Get-Content dockagents-windows-x86_64-0.1.0+build.42.a1b2c3d.zip.sha256).Split(' ')[0]
$actual   = (Get-FileHash dockagents-windows-x86_64-0.1.0+build.42.a1b2c3d.zip -Algorithm SHA256).Hash.ToLower()
$expected -eq $actual
```

## Building locally instead

If you'd rather not pull a pre-built binary, the standard cargo flow works
on any of the supported platforms:

```bash
git clone https://github.com/MrTigerST/dockagents
cd dockagents
cargo install --path .
```

That installs `dockagents` into `~/.cargo/bin` (which should already be on
your `PATH` if you have `rustup` installed).
