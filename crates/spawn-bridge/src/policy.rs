use spawn_types::event::{CheckpointReason, EventKind};

/// Decides which normalized events require a workspace checkpoint.
#[derive(Debug, Default, Clone, Copy)]
pub struct CheckpointPolicy;

impl CheckpointPolicy {
    /// Return the checkpoint reason when an event must trigger a commit.
    #[must_use]
    pub fn reason_for(kind: &EventKind) -> Option<CheckpointReason> {
        match kind {
            EventKind::ToolCompleted { .. } => Some(CheckpointReason::ToolCompleted),
            EventKind::TurnStopped { .. } => Some(CheckpointReason::TurnFinished),
            EventKind::ExecutionExited { .. } | EventKind::ExecutionLost { .. } => {
                Some(CheckpointReason::ExecutionShutdown)
            }
            _ => None,
        }
    }
}
