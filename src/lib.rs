//! DockAgents — distributable multi-agent sandboxes.
//!
//! Layout:
//!   * [`cli`] — `dockagents` command-line interface (clap + dispatch).
//!   * [`manifest`] — YAML manifest schema and parsing.
//!   * [`paths`] — host paths under `~/.dockagents/`.
//!   * [`registry`] — local file-backed package store (Phase 2 stub).
//!   * [`runtime`] — process manager, message bus, workspace + mount handling.
//!   * [`agent`] — subprocess entrypoint that drives a single LLM agent.

pub mod agent;
pub mod api;
pub mod cli;
pub mod config;
pub mod isolation;
pub mod manifest;
pub mod mcp;
pub mod paths;
pub mod registry;
pub mod remote;
pub mod runtime;
pub mod signing;
pub mod sip;
pub mod watcher;

pub use manifest::Manifest;
