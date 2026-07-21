//! The `code_context` tool — token-cheap codebase lookups (summary / search /
//! context) so the agent reads fewer whole files. Lexical, index-free, and
//! `.gitignore`-aware; a lightweight, grok-native take on an OpusDei-style
//! context layer.

pub mod outline;
pub mod search;
pub mod tool;
pub mod types;

pub use tool::{CODE_CONTEXT_TOOL_NAME, CodeContextTool};
