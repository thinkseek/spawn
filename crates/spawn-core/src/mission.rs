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
}

impl TaskState {
    /// All states in the order of the SQL check constraint.
    pub const ALL: [Self; 6] = [
        Self::Assigned,
        Self::Delivered,
        Self::Reported,
        Self::Revise,
        Self::Accepted,
        Self::Cancelled,
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

#[cfg(test)]
mod tests {
    use super::TaskState;

    #[test]
    fn each_state_is_pinned_to_its_sql_value() {
        let expected = [
            (TaskState::Assigned, "assigned"),
            (TaskState::Delivered, "delivered"),
            (TaskState::Reported, "reported"),
            (TaskState::Revise, "revise"),
            (TaskState::Accepted, "accepted"),
            (TaskState::Cancelled, "cancelled"),
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
}
