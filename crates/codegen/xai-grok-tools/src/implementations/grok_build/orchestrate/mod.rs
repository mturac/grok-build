//! `orchestrate` — a deterministic fan-out / verify tool.
//!
//! Fans N sub-tasks out concurrently over the existing `SubagentBackend`,
//! barriers on all of them, then gathers via an optional synthesize/verify step.
//! A thin, deterministic layer over `Task`'s spawn path — the fan-out shape is
//! fixed at call time. See `docs/superpowers/specs/2026-07-20-orchestrate-fanout-verify-design.md`.

pub mod fanout;
pub mod gather;
pub mod tool;
pub mod types;

pub use tool::{ORCHESTRATE_TOOL_NAME, OrchestrateTool};
