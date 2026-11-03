use serde::{Deserialize, Serialize};

/// Native agent engine behind a logical session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// Claude Code driven through tmux and hooks.
    ClaudeCode,
    /// OpenAI Codex driven through `codex exec --json` or app-server.
    Codex,
}

/// Marker newtype for engine-specific strings.
pub type EngineKind = Engine;
