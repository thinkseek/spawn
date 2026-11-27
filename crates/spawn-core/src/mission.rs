//! Mission-layer vocabulary shared by the store and the CLI.
//!
//! The SQL check constraint on `mission_tasks.state` (spawn-postgres
//! `sql/schema.sql`) is the source of truth; [`TaskState`] mirrors it so Rust
//! code matches on variants instead of bare strings. spawn-postgres pins the
//! two against each other in a test.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// State of one mission task (`mission_tasks.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    /// Assigned by the supervisor, not delivered yet.
    Assigned,
    /// Delivered to its worker, being worked on.
    Delivered,
    /// The worker's report is in, waiting for review.
    Reported,
    /// Sent back by the reviewer, to be delivered again.
    Revise,
    /// Accepted by the reviewer.
    Accepted,
    /// Cancelled before it was accepted.
    Cancelled,
    /// Passed its mission's gate; its work is being landed on the source.
    Landing,
    /// Its work landed on the mission's source.
    Landed,
}

impl TaskState {
    /// All states in the order of the SQL check constraint.
    pub const ALL: [Self; 8] = [
        Self::Assigned,
        Self::Delivered,
        Self::Reported,
        Self::Revise,
        Self::Accepted,
        Self::Cancelled,
        Self::Landing,
        Self::Landed,
    ];

    /// The SQL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::Delivered => "delivered",
            Self::Reported => "reported",
            Self::Revise => "revise",
            Self::Accepted => "accepted",
            Self::Cancelled => "cancelled",
            Self::Landing => "landing",
            Self::Landed => "landed",
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not one of the task states.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown task state: {0:?}")]
pub struct UnknownTaskState(pub String);

impl FromStr for TaskState {
    type Err = UnknownTaskState;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| UnknownTaskState(value.to_string()))
    }
}

const WORKSPACE_PREFIX: &str = "[workspace:";

/// Profile of the mission's workspace domain (`code`, `media`, `factory`, `general`).
///
/// Defaults to [`WorkspaceKind::Code`] so existing Git/code missions preserve
/// identical persistence, gate checks, and JSON output without schema migration.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceKind {
    /// Traditional software repository workspace with Git history and code gates.
    #[default]
    Code,
    /// Self-media content, graphic, and audio/video production pipeline.
    Media,
    /// Multi-step industrial or data automation production pipeline.
    Factory,
    /// General document, operations, and research collaboration workspace.
    General,
}

/// Static profile metadata derived from a [`WorkspaceKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceProfile {
    /// Canonical workspace kind identifier.
    pub kind: WorkspaceKind,
    /// Short operator-facing description of the pipeline domain.
    pub summary: &'static str,
    /// Whether this workspace kind expects a Rust/Git code gate runner by default.
    pub expects_code_gate: bool,
    /// Operator hint explaining acceptance and gating expectations for this kind.
    pub gate_hint: &'static str,
}

impl WorkspaceKind {
    /// All supported workspace kinds in canonical order.
    pub const ALL: [Self; 4] = [Self::Code, Self::Media, Self::Factory, Self::General];

    /// Canonical lowercase CLI/JSON identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Media => "media",
            Self::Factory => "factory",
            Self::General => "general",
        }
    }

    /// Whether this kind assumes a code/Git test gate when `--gate` is absent.
    #[must_use]
    pub const fn expects_code_gate(self) -> bool {
        matches!(self, Self::Code)
    }

    /// Static domain profile and gating guidance for this workspace kind.
    #[must_use]
    pub const fn profile(self) -> WorkspaceProfile {
        match self {
            Self::Code => WorkspaceProfile {
                kind: self,
                summary: "Code repository workspace",
                expects_code_gate: true,
                gate_hint: "Uses Git/code gate scenarios (e.g. scripts/rsi/gate.sh) when --gate is configured.",
            },
            Self::Media => WorkspaceProfile {
                kind: self,
                summary: "Self-media content & audio/video asset pipeline",
                expects_code_gate: false,
                gate_hint: "Non-code media workspace: omit --gate for review-based acceptance or supply media-asset/format validation scenarios instead of Rust/Git code test gates.",
            },
            Self::Factory => WorkspaceProfile {
                kind: self,
                summary: "Multi-step industrial/data automation pipeline",
                expects_code_gate: false,
                gate_hint: "Non-code factory pipeline: omit --gate for review-based acceptance or supply deterministic pipeline/output validators instead of Rust/Git code test gates.",
            },
            Self::General => WorkspaceProfile {
                kind: self,
                summary: "General document & research collaboration workspace",
                expects_code_gate: false,
                gate_hint: "Non-code general workspace: review-based acceptance is recommended; no Rust/Git code test gate is required.",
            },
        }
    }

    /// Format mission `text` carrying this workspace kind without requiring
    /// database schema changes. [`WorkspaceKind::Code`] leaves `text` untouched.
    #[must_use]
    pub fn encode_mission_text(self, text: &str) -> String {
        let (_, body) = Self::parse_mission_text(text);
        if self == Self::Code {
            body.to_string()
        } else {
            format!("{WORKSPACE_PREFIX}{}] {}", self.as_str(), body.trim_start())
        }
    }

    /// Extract the [`WorkspaceKind`] and underlying mission body from stored
    /// mission text, defaulting to [`WorkspaceKind::Code`] when untagged.
    #[must_use]
    pub fn parse_mission_text(text: &str) -> (Self, &str) {
        let trimmed = text.trim_start();
        if let Some(after_prefix) = trimmed.strip_prefix(WORKSPACE_PREFIX)
            && let Some((raw_kind, rest)) = after_prefix.split_once(']')
            && let Ok(kind) = raw_kind.parse::<Self>()
        {
            return (kind, rest.trim_start());
        }
        (Self::Code, text)
    }
}

impl fmt::Display for WorkspaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not one of the supported workspace kinds.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown workspace kind: {0:?} (expected one of: code, media, factory, general)")]
pub struct UnknownWorkspaceKind(pub String);

impl FromStr for WorkspaceKind {
    type Err = UnknownWorkspaceKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| UnknownWorkspaceKind(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{TaskState, WorkspaceKind};

    #[test]
    fn each_state_is_pinned_to_its_sql_value() {
        let expected = [
            (TaskState::Assigned, "assigned"),
            (TaskState::Delivered, "delivered"),
            (TaskState::Reported, "reported"),
            (TaskState::Revise, "revise"),
            (TaskState::Accepted, "accepted"),
            (TaskState::Cancelled, "cancelled"),
            (TaskState::Landing, "landing"),
            (TaskState::Landed, "landed"),
        ];
        assert_eq!(TaskState::ALL.len(), expected.len());
        for (state, sql) in expected {
            assert_eq!(state.as_str(), sql);
            assert_eq!(state.to_string(), sql);
            assert_eq!(sql.parse::<TaskState>(), Ok(state));
            assert_eq!(
                serde_json::to_value(state).expect("serializes"),
                serde_json::Value::String(sql.to_string())
            );
            assert_eq!(
                serde_json::from_value::<TaskState>(serde_json::Value::String(sql.to_string()))
                    .expect("deserializes"),
                state
            );
        }
    }

    #[test]
    fn unknown_values_are_errors() {
        assert!("Assigned".parse::<TaskState>().is_err());
        assert!("done".parse::<TaskState>().is_err());
        assert!(serde_json::from_str::<TaskState>("\"done\"").is_err());
    }

    #[test]
    fn workspace_kind_roundtrips_and_encodes_without_breaking_code_default() {
        assert_eq!(WorkspaceKind::default(), WorkspaceKind::Code);
        let expected = [
            (WorkspaceKind::Code, "code", true),
            (WorkspaceKind::Media, "media", false),
            (WorkspaceKind::Factory, "factory", false),
            (WorkspaceKind::General, "general", false),
        ];
        assert_eq!(WorkspaceKind::ALL.len(), expected.len());
        for (kind, tag, expects_code_gate) in expected {
            assert_eq!(kind.as_str(), tag);
            assert_eq!(kind.to_string(), tag);
            assert_eq!(tag.parse::<WorkspaceKind>(), Ok(kind));
            assert_eq!(kind.expects_code_gate(), expects_code_gate);
            assert_eq!(kind.profile().kind, kind);
            assert_eq!(kind.profile().expects_code_gate, expects_code_gate);
            assert_eq!(
                serde_json::to_value(kind).expect("serializes"),
                serde_json::Value::String(tag.to_string())
            );
            assert_eq!(
                serde_json::from_value::<WorkspaceKind>(serde_json::Value::String(tag.to_string()))
                    .expect("deserializes"),
                kind
            );

            let encoded = kind.encode_mission_text("produce weekly deliverables");
            let (parsed_kind, parsed_body) = WorkspaceKind::parse_mission_text(&encoded);
            assert_eq!(parsed_kind, kind);
            assert_eq!(parsed_body, "produce weekly deliverables");
        }

        // `Code` leaves plain and arbitrary bracketed text completely untouched.
        assert_eq!(
            WorkspaceKind::Code.encode_mission_text("[urgent] ship fix"),
            "[urgent] ship fix"
        );
        assert_eq!(
            WorkspaceKind::parse_mission_text("[urgent] ship fix"),
            (WorkspaceKind::Code, "[urgent] ship fix")
        );
        // Re-encoding strips an earlier `[workspace:...]` prefix idempotently.
        let media = WorkspaceKind::Media.encode_mission_text("cut clips");
        assert_eq!(
            WorkspaceKind::Factory.encode_mission_text(&media),
            "[workspace:factory] cut clips"
        );
        assert!("Media".parse::<WorkspaceKind>().is_err());
        assert!("unknown".parse::<WorkspaceKind>().is_err());
    }
}
