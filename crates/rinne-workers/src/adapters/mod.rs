//! Per-CLI and per-API adapters that implement [`rinne_core::Worker`]
//! (`CONTEXT.md` §8, §16).
//!
//! The Phase 2 set: Claude Code, Codex, and OpenCode (harness, over
//! `subprocess-json`), plus one OpenAI-compatible API worker (over `http`).
//! More adapters (Cursor, Grok Build, Aider, Antigravity) are V2.

pub mod aider;
pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod common;
pub mod cursor;
pub mod grok;
pub mod opencode;
pub mod openai_api;

pub use common::HarnessAdapter;
pub use openai_api::OpenAiWorker;
