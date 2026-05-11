# DockAgents

> Isolated, distributable, invocable multi-agent environments — via CLI or from an LLM.

**Runtime:** Rust · **Config:** YAML manifest · **Distribution:** Centralized registry · **Protocol:** SIP

---

## Table of Contents

1. [Vision](#1-vision)
2. [Core Concepts](#2-core-concepts)
3. [System Architecture](#3-system-architecture)
4. [The YAML Manifest](#4-the-yaml-manifest)
5. [Rust Runtime](#5-rust-runtime)
6. [Registry](#6-registry)
7. [Sandbox Invocation Protocol (SIP)](#7-sandbox-invocation-protocol-sip)
8. [Entry Points](#8-entry-points)
9. [Filesystem and Mounts](#9-filesystem-and-mounts)
10. [Security](#10-security)
11. [Use Cases](#11-use-cases)
12. [Comparison with Existing Systems](#12-comparison-with-existing-systems)
13. [Roadmap](#13-roadmap)

---

## 1. Vision

DockAgents is a platform for managing **distributable multi-agent sandboxes**. Each sandbox is an isolated environment containing one or more pre-configured AI agents, a dedicated filesystem, and external mounts toward the user's desktop.

Sandboxes are **distributable products** — not tools to configure. Whoever publishes `indie-devteam` on the registry defines once the agent composition, skills, files, and permissions. Whoever installs it runs a single command and finds the team ready to go.

The system exposes two distinct entry points converging on the same runtime:

- **CLI** — the human user installs, runs, and interacts with sandboxes directly
- **LLM orchestrator** — an AI agent invokes sandboxes as external capabilities, reading the manifest to understand I/O schema and execution mode

The name `DockAgents` reflects this duality: `crate` in Rust means distributable package, and that is exactly what every sandbox is — a packaged, versioned, installable artifact containing a complete agent team.

### Core differentiator

> Systems like Claude Code + skills distribute **instructions** to a single agent.  
> DockAgents distributes **complete environments** with multiple agents in deliberate tension, persistent state, and isolated filesystems.

---

## 2. Core Concepts

### Sandbox

A sandbox is the fundamental unit of DockAgents. It is a self-contained environment defined by a YAML manifest and composed of three invariant elements:

| Element | Description |
|---|---|
| **Agents (N × yaml)** | Isolated processes with their own identity, skill, model, and system prompt |
| **Dedicated filesystem** | Workspace that persists between sessions, invisible to other sandboxes |
| **External mounts** | Bidirectional links to desktop folders or user paths |

### Agent

Each agent inside a sandbox is a separate OS process with its own context. Agents do not share context with each other — this is by design. A `security-auditor` must not see what the `senior-reviewer` is thinking before producing its output. This structural separation produces **genuine tension** between outputs, not a single mind simulating disagreement.

### Persistent vs Ephemeral Sandbox

| | Persistent | Ephemeral |
|---|---|---|
| **Lifecycle** | `manifest: persistent` | `manifest: ephemeral` |
| **State** | Survives between sessions | Destroyed after output |
| **Filesystem** | Permanent workspace | Temporary, removed on exit |
| **Local cache** | Installed once | Pulled on demand, cacheable |
| **Typical use** | Installed by the user | Invoked by another sandbox via SIP |

### Registry

The registry is a centralized catalog of sandbox packages. It serves two audiences simultaneously:

- **Human users** — an app store where you search, browse, and install sandboxes
- **LLM orchestrators** — a capability catalog where agents discover and invoke tools at runtime

---

## 3. System Architecture

The system is structured in four stacked layers with well-defined interfaces.

```
┌─────────────────────────────────────────────────────────┐
│                     ENTRY POINTS                        │
│                                                         │
│   Human user (CLI)          LLM orchestrator            │
│   sandboxd install/run      tool call / MCP / API       │
└───────────────┬─────────────────────────┬───────────────┘
                │                         │
                ▼                         ▼
┌─────────────────────────────────────────────────────────┐
│                       REGISTRY                          │
│          registry.DockAgents.com                        │
│    manifest store · artifact store · semver             │
└───────────────────────────┬─────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────┐
│                   RUST RUNTIME                          │
│       process manager · message bus · SIP handler       │
└──────┬─────────────────┬──────────────────┬─────────────┘
       │                 │                  │
       ▼                 ▼                  ▼
┌────────────┐   ┌────────────┐   ┌────────────────────┐
│  Agent A   │   │  Agent B   │   │     Agent N        │
│  (process) │   │  (process) │   │     (process)      │
│  workspace │   │  workspace │   │     workspace      │
└─────┬──────┘   └─────┬──────┘   └──────────┬─────────┘
      │                │                      │
      └────────────────┴──────────────────────┘
                       │
                       ▼
              ┌─────────────────┐
              │   Mount point   │
              │  ~/Desktop/...  │
              └─────────────────┘
```

### Layer responsibilities

**Entry Points** — Two independent interfaces converging on the same runtime. The CLI interprets human commands including natural language installation. The LLM interface reads manifests to understand invocation contracts before calling.

**Registry** — Stores manifest files, versioned tarballs, and I/O schemas. Exposes a REST API used by both the CLI (for install/pull) and the runtime (for SIP on-demand pulls). Supports semantic search for natural language queries.

**Rust Runtime** — The core of the system. Responsible for spawning and supervising agent processes, managing inter-agent message passing within a sandbox, handling SIP invocations (including ephemeral sandbox lifecycle), and enforcing filesystem isolation.

**Sandbox** — The instantiated environment. Each agent runs as a separate OS process. The runtime coordinates them through a message bus but never merges their contexts.

---

## 4. The YAML Manifest

The manifest is the central contract of the system. It is read by the CLI for installation, by the runtime for startup, and by LLMs for invocation. Every sandbox is defined by exactly one manifest.

```yaml
name:        indie-devteam
version:     1.0.0
description: Code review team for indie developers
lifecycle:   persistent   # persistent | ephemeral

execution:
  mode:      sync         # sync | async | fire-and-forget
  timeout:   120s
  input:
    - type: directory
      accepts: [py, ts, rs, go, js]
  output:
    - type: structured_json
      schema: ./schemas/review-output.json

agents:
  - id:        senior-reviewer
    model:     claude-sonnet-4-20250514
    skill:     ./skills/senior-reviewer.md
    workspace: ./workspaces/reviewer/

  - id:        security-auditor
    model:     claude-sonnet-4-20250514
    skill:     ./skills/security-auditor.md
    workspace: ./workspaces/security/

  - id:        pm-planner
    model:     claude-sonnet-4-20250514
    skill:     ./skills/pm-planner.md
    workspace: ./workspaces/planner/

mounts:
  - host:    ~/Desktop/indie-devteam/
    sandbox: /output/
    mode:    readwrite     # readonly | readwrite

capabilities:
  invoke:
    - cve-lookup
    - dependency-auditor

message_bus:
  topology: broadcast     # broadcast | directed | none
  visibility: post_output # agents see each other's output only after writing their own
```

### Manifest fields

| Field | Required | Description |
|---|---|---|
| `name` | ✅ | Unique identifier on the registry |
| `version` | ✅ | Semver string |
| `lifecycle` | ✅ | `persistent` or `ephemeral` |
| `execution.mode` | ✅ | How the caller interacts with results |
| `execution.timeout` | ✅ | Max wall time before forced termination |
| `agents` | ✅ | List of agent definitions (minimum 1) |
| `mounts` | ❌ | Bidirectional filesystem bridges |
| `capabilities.invoke` | ❌ | Sandbox names this sandbox may call via SIP |
| `message_bus` | ❌ | Inter-agent coordination topology |

### Execution modes

**`sync`** — The caller blocks until the sandbox produces its full output. Used when the result is needed before continuing.

**`async`** — The caller continues immediately and registers a callback. The runtime notifies when output is ready.

**`fire-and-forget`** — The caller delegates entirely. Output is written to the sandbox filesystem or mount. No notification.

The execution mode is declared in the manifest — the caller does not choose it. This means an LLM orchestrator reads the manifest first and adapts its behavior accordingly.

---

## 5. Rust Runtime

The runtime is the engine that brings manifests to life. It is written in Rust for performance, safety, and native process management.

### Responsibilities

**Process management** — Each agent defined in the manifest is spawned as a separate OS process. The runtime supervises process health, enforces timeouts, and handles crashes gracefully.

**Filesystem isolation** — Each agent has a dedicated workspace directory. The runtime enforces that agents cannot access each other's workspaces. Shared access is possible only through explicitly declared mount points.

**Message bus** — Inter-agent communication within a sandbox flows through a runtime-managed message bus. The topology (broadcast, directed, none) is declared in the manifest. By default, agents operate in isolation and only see each other's outputs after writing their own — preventing premature convergence.

**SIP handler** — When an agent emits a SIP invocation request, the runtime intercepts it, resolves the target sandbox from the registry, pulls if needed, spawns the ephemeral sandbox, manages I/O, and returns the result to the requesting agent.

**Mount bridge** — The runtime manages bidirectional filesystem bridges between sandbox directories and host paths (e.g., the user's Desktop). Permissions (readonly/readwrite) are enforced at the OS level.

### Process lifecycle

```
sandboxd run indie-devteam --input ./src/auth/
    │
    ├── read manifest
    ├── validate input against schema
    ├── spawn agent processes (parallel)
    │       ├── senior-reviewer  (pid 4821)
    │       ├── security-auditor (pid 4822)
    │       └── pm-planner       (pid 4823)
    │
    ├── [SIP invocation from security-auditor]
    │       ├── pull cve-lookup@^1.0 from registry
    │       ├── spawn ephemeral sandbox
    │       ├── pass input, receive output
    │       └── destroy ephemeral sandbox
    │
    ├── collect agent outputs
    ├── synthesize final report
    ├── write to mount ~/Desktop/indie-devteam/report.md
    └── exit (persistent sandbox state preserved)
```

---

## 6. Registry

The registry is a centralized package server for DockAgents sandboxes. It is conceptually similar to crates.io (Rust) or npm (Node), but the packages are complete agent environments rather than code libraries.

### Package structure

Every published sandbox is a versioned tarball containing:

```
indie-devteam-1.0.0.tar.gz
├── manifest.yaml
├── skills/
│   ├── senior-reviewer.md
│   ├── security-auditor.md
│   └── pm-planner.md
├── workspaces/          # initial workspace state (can be empty)
├── schemas/
│   └── review-output.json
└── README.md
```

### Registry API

```
GET  /search?q=code+review          # semantic search
GET  /packages/:name                # package metadata
GET  /packages/:name/:version       # specific version manifest
GET  /packages/:name/:version/pull  # download tarball
POST /publish                       # publish new package
```

### Semantic search

The registry supports natural language queries. Both the CLI (`sandboxd install "code review team for indie developers"`) and LLM orchestrators use the same search endpoint to discover relevant sandboxes. The engine performs embedding-based similarity matching against manifest descriptions and README content.

### Versioning

Packages follow semver strictly. The SIP protocol uses semver ranges for invocation:

```yaml
capabilities:
  invoke:
    - cve-lookup@^1.0    # any 1.x compatible version
    - translator@~2.1    # 2.1.x only
```

### Package signing

Every package published to the registry is signed with the publisher's key. The runtime verifies signatures before installing or pulling any package, including ephemeral SIP invocations.

---

## 7. Sandbox Invocation Protocol (SIP)

SIP is the protocol that enables a sandbox to invoke another sandbox as an external capability. It is the mechanism behind ephemeral sandboxes and the composability of the system.

### Concept

A running sandbox can declare that it needs a capability it does not have internally. Instead of failing, it emits a SIP invocation request. The runtime resolves the target, pulls it if necessary, runs it in isolation, and returns the structured output to the requesting agent.

```
legal-team sandbox
    │
    ├── contract-reader  ──→  processes PDF
    ├── risk-analyzer    ──→  finds German clause
    │                        needs translation
    │                        emits SIP: invoke translator@^1.0
    │                              │
    │                         ┌────▼────────────────────┐
    │                         │  EPHEMERAL SANDBOX       │
    │                         │  translator@1.2.0        │
    │                         │  pulled from registry    │
    │                         │  executes in isolation   │
    │                         │  returns translated text │
    │                         │  destroyed after output  │
    │                         └─────────────────────────┘
    │                              │
    │                         output returned to risk-analyzer
    └── summary-writer   ──→  writes final report
```

### SIP invocation declaration

A sandbox must declare which sandboxes it is allowed to invoke in its manifest:

```yaml
capabilities:
  invoke:
    - cve-lookup
    - translator
    - dependency-auditor
```

Invoking a sandbox not declared in `capabilities.invoke` is rejected by the runtime.

### SIP invocation payload

When an agent emits a SIP request, it uses a structured format:

```yaml
invoke:
  sandbox:   cve-lookup
  version:   "^1.0"
  lifecycle: ephemeral
  timeout:   30s
  input:
    cve_query: "JWT missing expiry validation"
  output_schema: ./schemas/cve-result.json
```

### Execution modes in SIP

The execution mode of an invoked sandbox is read from its own manifest, not chosen by the caller. This means:

- If `cve-lookup` declares `execution.mode: sync` — the invoking agent blocks until it receives the CVE data
- If `translator` declares `execution.mode: async` — the invoking agent can continue processing other files and receives the translation via callback
- If a logging sandbox declares `execution.mode: fire-and-forget` — the invoking agent delegates and moves on immediately

### Local cache

Cold-starting an ephemeral sandbox on every invocation would be prohibitively slow. The runtime maintains a local cache keyed by package name and version. A sandbox invoked with `cve-lookup@^1.0` is pulled once and reused until a newer compatible version is available on the registry.

```
cache hit:  invoke cve-lookup@^1.0  →  already at 1.2.0, use cached
cache miss: invoke translator@^2.0  →  pull 2.1.3, cache it, run
invalidate: registry releases 1.3.0 →  cache invalidated for ^1.0 range
```

---

## 8. Entry Points

### 8.1 CLI for human users

The CLI is the primary interface for human users. It is designed to be usable by both developers and non-developers.

```bash
# Install by exact name
sandboxd install indie-devteam

# Install by natural language description
sandboxd install "code review team for indie developers"

# Run a sandbox
sandboxd run indie-devteam --input ./src/auth/

# Run with explicit output path
sandboxd run legal-team --input ./contracts/ --output ~/Desktop/legal-output/

# List installed sandboxes
sandboxd list

# Show sandbox status and agent processes
sandboxd status indie-devteam

# Stop a running sandbox
sandboxd stop indie-devteam

# Publish a sandbox to the registry
sandboxd publish ./my-sandbox/

# Search the registry
sandboxd search "document analysis"

# Pull a specific version without running
sandboxd pull cve-lookup@1.2.0
```

### 8.2 LLM invocation

LLMs interact with DockAgents through two interfaces:

**MCP server** — DockAgents exposes a Model Context Protocol server. Any MCP-compatible LLM (Claude, GPT-4o, Gemini) can discover and invoke sandboxes natively without custom integration.

**REST API** — Direct HTTP invocation for orchestrators that manage their own tool-calling logic.

```http
# Discover sandboxes
GET https://registry.DockAgents.com/search?q=code+review
→ returns manifest list with I/O schemas

# Invoke a sandbox
POST https://runtime.DockAgents.com/invoke
Content-Type: application/json

{
  "sandbox": "indie-devteam",
  "version": "^1.0",
  "input": {
    "directory_content": "..."
  }
}

# Response (sync mode)
{
  "sandbox": "indie-devteam",
  "version": "1.0.3",
  "execution_time_ms": 8420,
  "output": {
    "review": "...",
    "security": "...",
    "scope": "..."
  }
}
```

The LLM reads the manifest before invoking. It knows the expected input format, the output schema, and the execution mode. No inference required — the contract is declared.

### 8.3 Natural language installation

The `sandboxd install` command accepts both exact names and natural language descriptions. The registry's semantic search engine finds the best match and presents it for confirmation before installing.

```
$ sandboxd install "something that analyzes PDF contracts and identifies risks"

Searching registry...

Found: legal-team@2.1.0
  contract-reader · risk-analyzer · summary-writer
  "Legal document analysis pipeline for contract review"
  ★ 4.8  Downloads: 12,400

Install legal-team@2.1.0? [y/N] y
Pulling package... ✓
Verifying signature... ✓
Installing agents... ✓

Ready. Run with: sandboxd run legal-team --input ./contracts/
```

---

## 9. Filesystem and Mounts

### Sandbox filesystem structure

Every installed sandbox has a dedicated directory on the host filesystem:

```
~/.DockAgents/sandboxes/indie-devteam/
├── manifest.yaml
├── skills/
│   ├── senior-reviewer.md
│   ├── security-auditor.md
│   └── pm-planner.md
├── workspaces/
│   ├── reviewer/          ← agent A workspace (persistent)
│   ├── security/          ← agent B workspace (persistent)
│   └── planner/           ← agent C workspace (persistent)
├── output/                ← synthesized output
└── .state/                ← runtime state, logs
```

Each agent's workspace is its private, persistent storage. The runtime enforces that Agent A cannot read or write Agent B's workspace. This isolation is enforced at the OS level, not just by convention.

### Mount points

Mounts create bidirectional bridges between the sandbox filesystem and host paths. They are declared in the manifest and managed by the runtime.

```yaml
mounts:
  - host:    ~/Desktop/indie-devteam/
    sandbox: /output/
    mode:    readwrite

  - host:    ~/Documents/contracts/
    sandbox: /input/contracts/
    mode:    readonly
```

**`readonly`** — The sandbox can read files from the host path but cannot write. Useful for input directories.

**`readwrite`** — The sandbox can read and write. Changes are immediately visible on the host. This is the mechanism behind the "drop files in a folder and get results back" UX.

### Desktop UX

The mount system enables a zero-terminal UX for non-developer users:

1. User installs a sandbox once via CLI
2. A folder appears on their Desktop (e.g., `~/Desktop/legal-team/`)
3. User drops files into the `input/` subfolder
4. Runs the sandbox (or it runs automatically via a file watcher)
5. Results appear in the `output/` subfolder
6. User opens results in Finder/Explorer — no terminal needed after step 1

---

## 10. Security

### Process isolation

Each agent runs as a separate OS process. The runtime uses OS-level mechanisms to enforce isolation:

- **Linux**: Bubblewrap namespaces + Landlock filesystem restrictions + seccomp syscall filtering
- **macOS**: Seatbelt (sandbox-exec) profiles per agent process

### Ephemeral sandbox zero-trust

Ephemeral sandboxes invoked via SIP are treated as untrusted by default:

- **No access to the invoking sandbox's filesystem** — the ephemeral sandbox receives only the data explicitly passed in the SIP input payload
- **No network access by default** — network permissions must be explicitly declared in the manifest
- **Mandatory timeout** — every ephemeral sandbox has a declared timeout; the runtime kills it if exceeded
- **Signature verification** — the runtime verifies the cryptographic signature of every pulled package before execution, including SIP invocations

### Registry trust model

```
Publisher signs package with private key
    │
    ▼
Registry stores package + signature + public key
    │
    ▼
Runtime verifies signature before install/pull
    │
    ▼
Package executes only if signature is valid
```

### Capability declaration

A sandbox can only invoke sandboxes explicitly listed in its `capabilities.invoke` field. Attempting to invoke an undeclared sandbox at runtime raises an error and halts execution. This prevents sandbox escapes through unexpected SIP chains.

---

## 11. Use Cases

### Code review for indie developers

**Sandbox:** `indie-devteam`  
**Agents:** `senior-reviewer` · `security-auditor` · `pm-planner`

A developer runs `sandboxd run indie-devteam --input ./src/auth/`. Three processes start in parallel, each reading the same code with different lenses. `security-auditor` finds a JWT without expiry validation and invokes `cve-lookup` as an ephemeral sandbox to get the CVE reference. Three independent reports land in `~/Desktop/indie-devteam/`. The conflict between `scope.md` (missing refresh token) and `review.md` (structure looks fine) is real information — not a single mind pretending to disagree.

### Legal document analysis

**Sandbox:** `legal-team`  
**Agents:** `contract-reader` · `risk-analyzer` · `summary-writer`

A small firm drops 50 PDF contracts into a desktop folder. `contract-reader` extracts clauses, `risk-analyzer` flags anomalies, `summary-writer` produces the briefing. When a German-language contract appears, `risk-analyzer` invokes `translator` as an ephemeral sandbox, gets the translated text, and continues. The firm's paralegal finds ready-to-review summaries without touching a terminal.

### Scientific research synthesis

**Sandbox:** `research-lab`  
**Agents:** `paper-reader` · `critic-agent` · `synthesis-writer`

The sandbox accumulates a knowledge base in its persistent workspace over weeks. `paper-reader` extracts contributions from new PDFs, `critic-agent` cross-references them against everything already read, `synthesis-writer` updates a living document. A month later, the researcher has a synthesis document that remembers every paper ever processed — impossible with a stateless skill.

### Content pipeline for agencies

**Sandbox:** `content-studio`  
**Agents:** `researcher` · `copywriter` · `seo-editor`

The client drops briefs into a shared folder. The sandbox produces drafts, optimizes for SEO, adapts for different channels. The account manager reviews the output folder. The only technical interaction was `sandboxd install content-studio` on day one.

### LLM composing capabilities at runtime

An LLM orchestrator receives: *"analyze this dataset, find anomalies, write an executive report, translate it into three languages."* It queries the registry, discovers `data-analyst`, `report-writer`, and `translator`. It invokes `data-analyst` (sync), passes the output to `report-writer` (sync), then fires three parallel async invocations of `translator`. None of these capabilities existed in the orchestrator's initial context — they were discovered and composed at runtime.

---

## 12. Comparison with Existing Systems

| Dimension | Claude Code + Skills | Multi-agent Claude Code | **DockAgents** |
|---|---|---|---|
| Agent identity | Single (skill changes behavior) | Multiple dynamic (no fixed persona) | **Multiple persistent (yaml per agent)** |
| Isolation | None (same process) | Partial (shared context) | **Full (process + filesystem)** |
| Coordination | None (sequential) | Implicit (CC orchestrator) | **Explicit (message bus)** |
| Filesystem | Current project | Current project | **Dedicated workspace + desktop mount** |
| Distribution | Local .md files | Hooks + CLAUDE.md | **Centralized registry, versioned, signed** |
| State between sessions | No (dies with terminal) | No (task-bound) | **Yes (persistent filesystem)** |
| LLM invocable | No | No | **Yes (MCP + REST API)** |
| Inter-sandbox invocation | No | No | **Yes (SIP)** |
| Target user | Developer (inside IDE) | Advanced developer | **Developer + non-developer** |

### What skills.sh and APM solve (and where they stop)

Tools like Paks, APM, and skills.sh distribute **instructions** — markdown files that change how a single agent behaves. They are excellent for giving an agent domain knowledge on demand. They fall short when:

- The work requires **roles in genuine tension** — a reviewer and a writer that are the same process cannot truly disagree
- The workflow needs **state that accumulates** between sessions — skills are stateless
- The **volume exceeds a context window** — a persistent sandbox workspace has no token limits
- **Parallelism matters** — skills run sequentially inside one context; DockAgents agents run as separate OS processes

---

## 13. Roadmap

### Phase 1 — Core runtime

- [ ] CLI: `install`, `run`, `list`, `stop`, `status`
- [ ] YAML manifest parser with schema validation
- [ ] Rust process manager: spawn agents as separate OS processes
- [ ] Filesystem isolation and desktop mount bridge
- [ ] Message bus: broadcast and directed topologies
- [ ] Basic logging and crash recovery

### Phase 2 — Registry and distribution

- [ ] Centralized registry with REST API
- [ ] Package tarball format with manifest + skills + schemas
- [ ] Semantic search for natural language installation
- [ ] Cryptographic package signing and verification
- [ ] Semver dependency management
- [ ] `sandboxd publish` command and publisher tooling

### Phase 3 — SIP and LLM integration

- [ ] Sandbox Invocation Protocol (SIP) full implementation
- [ ] Ephemeral sandbox lifecycle: pull, spawn, return output, destroy
- [ ] Local cache with semver-based invalidation
- [ ] MCP server exposure for native LLM integration
- [ ] REST API for direct orchestrator invocation
- [ ] Zero-trust security invariants for ephemeral sandboxes

### Phase 4 — Ecosystem

- [ ] Web UI for registry browsing
- [ ] Sandbox analytics (usage, performance, error rates)
- [ ] Multi-model support (GPT-4o, Gemini, local via Ollama)
- [ ] File watcher for automatic sandbox triggering on desktop folder changes
- [ ] Sandbox composition: import agents from other sandboxes

---

## Appendix A — Glossary

| Term | Definition |
|---|---|
| **Sandbox** | A complete isolated environment containing agents, a filesystem, and mount definitions |
| **Agent** | A separate OS process running an LLM with a specific skill and workspace |
| **Manifest** | The YAML file that fully defines a sandbox — its agents, execution mode, mounts, and capabilities |
| **Registry** | The centralized server hosting published sandbox packages |
| **Crate** | A versioned, signed package on the registry containing a complete sandbox definition |
| **SIP** | Sandbox Invocation Protocol — the mechanism for one sandbox to invoke another at runtime |
| **Ephemeral sandbox** | A sandbox pulled from the registry at invocation time, executed, and destroyed after returning output |
| **Persistent sandbox** | A sandbox with a permanent local installation and state that survives between sessions |
| **Mount** | A bidirectional filesystem bridge between a sandbox directory and a host path |
| **Message bus** | The runtime-managed communication channel between agents within the same sandbox |

---

## Appendix B — Package layout

```
my-sandbox-1.0.0/
├── manifest.yaml          ← required
├── README.md              ← required for registry listing
├── skills/                ← agent skill files (markdown)
│   ├── agent-a.md
│   └── agent-b.md
├── workspaces/            ← initial workspace state
│   ├── agent-a/
│   └── agent-b/
├── schemas/               ← I/O JSON schemas
│   ├── input.json
│   └── output.json
└── .DockAgents/           ← runtime metadata (auto-generated)
    └── lock.yaml          ← resolved versions at install time
```

---

*DockAgents — DockAgents.com*
