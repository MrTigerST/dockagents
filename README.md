<p align="center">
  <img src="https://github.com/MrTigerST/dockagents/raw/main/dockagents-logo.png" width="200" />
</p>

<h1 align="center">DockAgents</h1

> Isolated, distributable, invocable multi-agent environments — via CLI or
> from an LLM.

## Our official website : https://dockagents.net

## What's in this repo

| Module | Purpose |
|---|---|
| `src/manifest.rs` | YAML manifest: lifecycle · execution · agents · mounts · capabilities · message_bus · per-agent `llm:` block. |
| `src/runtime/workspace.rs` | Per-agent workspace dirs, host mount bridges, synthesized output report. |
| `src/runtime/process.rs` | Spawns each agent as its own OS process; routes `@@BUS@@` and `@@SIP@@` lines. |
| `src/runtime/bus.rs` | Inter-agent message bus (`broadcast` / `directed` / `none`, `live` / `post_output`). |
| `src/agent.rs` | Agent subprocess — calls the LLM endpoint declared in the manifest. |
| `src/sip.rs` | Sandbox Invocation Protocol — validates against `capabilities.invoke`, runs ephemeral sandbox, delivers response to the calling agent's inbox. |
| `src/signing.rs` | Ed25519 publisher signing and verification (`dockagents keygen`, sign on publish, verify on install). |
| `src/isolation.rs` | OS-level isolation: bwrap (Linux), sandbox-exec (macOS), process boundaries (Windows). |
| `src/registry.rs` | Local file-backed registry. |
| `src/remote.rs` | HTTP(S) registry client — pack, sign, publish, pull, verify. |
| `src/mcp.rs` | MCP server over stdio (`dockagents mcp`). |
| `src/api.rs` | REST API for orchestrators (`dockagents serve`). |
| `src/watcher.rs` | File-watcher driven re-runs (`dockagents watch`). |
| `src/updater.rs` | GitHub Releases update checks, SHA-256 verification, and self-update install. |
| `src/cli.rs` | All `dockagents <subcommand>` dispatch. |

## Install
Go to the [Releases Page](https://github.com/MrTigerST/dockagents/releases)
and download the `dockagents-setup-*` executable for your operating system.
The setup executable installs `dockagents` and asks whether to add the install
directory to your OS `PATH`.

Portable `.zip` / `.tar.gz` archives are also published for manual installs.

## Updates

DockAgents checks GitHub Releases once per day and prints a notice when a
newer OS-specific executable is available. To check or install immediately:

```bash
dockagents update --check
dockagents update --yes
```

To opt in to automatic installs whenever the daily check finds a newer
release:

```bash
dockagents config set-updates --auto-install true
```

Set `DOCKAGENTS_NO_UPDATE_CHECK=1` to disable update checks for a shell, or
use `dockagents config set-updates --check false` to turn them off globally.

## Build

You need a Rust toolchain (1.75+ recommended). On Windows:

```powershell
winget install Rustlang.Rustup
rustup default stable
```

Then:

```bash
cargo build --release
# binary lands at: ./target/release/dockagents
```

## Quickstart

```bash
export ANTHROPIC_API_KEY=sk-ant-...

# Publish the example sandbox into your local registry.
./target/release/dockagents publish ./examples/indie-devteam

# Install it into ~/.dockagents/sandboxes/indie-devteam/
./target/release/dockagents install indie-devteam

# Run it on a directory of code.
./target/release/dockagents run indie-devteam --input ./src/

# Inspect status, stop running agents, see the manifest.
./target/release/dockagents list
./target/release/dockagents status indie-devteam
./target/release/dockagents stop indie-devteam
./target/release/dockagents manifest indie-devteam
```

### Talking to a remote registry

`install`, `pull`, `search`, and `publish` accept `--registry <URL>`, or pick
up `DOCKAGENTS_REGISTRY_URL` from the environment. Bearer auth via
`DOCKAGENTS_REGISTRY_TOKEN`.

```bash
export DOCKAGENTS_REGISTRY_URL=http://localhost:8787
dockagents publish ./examples/indie-devteam      # POST /publish
dockagents search "code review"                  # GET  /search?q=...
dockagents install indie-devteam                 # GET  /packages/... + /pull
dockagents install indie-devteam --version 0.1.0 # pin a specific version
```

Tarballs are sha256-verified against the manifest the registry returns
before the install root is touched.

## Manifest cheatsheet

```yaml
name:    indie-devteam
version: 0.1.0
lifecycle: persistent          # or ephemeral

execution:
  mode:    sync                # sync | async | fire-and-forget
  timeout: 240s

agents:
  - id:        senior-reviewer
    model:     claude-sonnet-4-5
    skill:     ./skills/senior-reviewer.md
    workspace: ./workspaces/reviewer/
    llm:                       # ⬅ user-declared LLM endpoint
      provider:    anthropic
      api_key_env: ANTHROPIC_API_KEY
      max_tokens:  4096

mounts:
  - host:    ~/Desktop/indie-devteam/
    sandbox: /output/
    mode:    readwrite

capabilities:
  invoke: []
  network: true

message_bus:
  topology:   broadcast
  visibility: post_output
```

### LLM configuration

Each agent declares its provider, endpoint, and credential reference:

| Field | Meaning |
|---|---|
| `provider` | `anthropic`, `openai`, `openai-compatible`, or any string the runner knows. Inferred from `model` prefix when absent. |
| `endpoint` | Full HTTPS URL. Defaults: Anthropic `messages`, OpenAI `chat/completions`. |
| `api_key_env` | Name of an env var holding the key. Preferred. |
| `api_key` | Literal key. Discouraged — convenient for local testing. |
| `api_version` | Provider-specific (Anthropic uses `anthropic-version`). |
| `max_tokens` | Output cap. |
| `extra_headers` | Additional static headers. |

You can mix providers per agent — e.g. one agent on Claude, another on a
local Ollama instance via `provider: openai-compatible` and a custom
`endpoint`.

## Subcommand quick reference

```text
dockagents install <name>     pull + extract a sandbox (local or remote)
dockagents run <name>         run an installed sandbox
dockagents list / status / stop / manifest
dockagents publish <path>     publish to local or remote registry
dockagents search <query>     ranked search
dockagents pull <name>        download without running
dockagents keygen             generate an Ed25519 publisher key
dockagents mcp                MCP server over stdio
dockagents serve              REST API for orchestrators
dockagents watch <name>       auto-rerun on host mount changes
dockagents update             install the latest GitHub Release executable
```

## Signing

```bash
dockagents keygen                                 # ~/.dockagents/keys/
dockagents publish ./examples/indie-devteam       # signs if a key exists
dockagents install indie-devteam --registry http://...   # verifies sig
DOCKAGENTS_REQUIRE_SIGNED=1 dockagents install ... # refuse unsigned
```

## OS-level isolation matrix

| Platform | Mechanism | Effect |
|---|---|---|
| Linux | `bwrap` (Bubblewrap) when on PATH | Per-agent mount namespace; only workspace + declared mounts are visible; net unshare when `capabilities.network=false` |
| macOS | `sandbox-exec` Seatbelt profile | File-write whitelist for workspace + readwrite mounts; net deny by default |
| Windows | Win32 Job Object | `KILL_ON_JOB_CLOSE` + `DIE_ON_UNHANDLED_EXCEPTION` + `ACTIVE_PROCESS=32` cap + UI restrictions (deny clipboard, desktop, exitWindows, system params, handle inheritance) |

All three are kernel-enforced and fail open with a warning if the facility
is unavailable.

## Streaming the REST API

```bash
curl -N -X POST http://127.0.0.1:8989/invoke?stream=true \
  -H 'Content-Type: application/json' \
  -d '{"sandbox":"indie-devteam","input":"hello"}'

event: run_started
data: {"event":"run_started","sandbox":"indie-devteam","version":"0.1.0",
       "agents":["senior-reviewer","security-auditor","pm-planner"]}

event: agent_spawned
data: {"event":"agent_spawned","agent":"senior-reviewer"}

event: agent_finished
data: {"event":"agent_finished","agent":"senior-reviewer","status":"ok",...}

event: run_finished
data: {"event":"run_finished","execution_time_ms":8412,"report_path":"..."}

event: end
data: {}
```

Drop `?stream=true` to get the buffered JSON response instead.

## Layout on disk

```
~/.dockagents/
  sandboxes/<name>/        installed (persistent) sandboxes
  cache/<name>/<run-id>/   ephemeral run roots
  registry/<name>/         local registry entries (publish target)
  state/                   per-process state (pidfiles, run logs)
```

The location can be overridden with `DOCKAGENTS_HOME=/some/other/path`.

## Testing

```bash
cargo test
```
