use spawn_types::clock::Timestamp;
use spawn_types::observation::{Confidence, SemanticState, WaitReason};

use crate::{Judgement, SessionSnapshot, StateJudge};

/// Deterministic offline judge used as fallback and as a test baseline.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicJudge;

impl HeuristicJudge {
    /// Create the stateless judge.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl StateJudge for HeuristicJudge {
    fn judge(&self, snapshot: &SessionSnapshot, _now: Timestamp) -> Result<Judgement, String> {
        let full = Confidence::new(1.0).map_err(|error| error.to_string())?;
        let high = Confidence::new(0.85).map_err(|error| error.to_string())?;

        if snapshot.execution_exited && !snapshot.execution_active {
            return Ok(Judgement {
                state: SemanticState::Completed,
                reason: None,
                confidence: full,
            });
        }
        if snapshot.permission_pending {
            return Ok(Judgement {
                state: SemanticState::Blocked,
                reason: Some(WaitReason::AwaitingUserPermission),
                confidence: full,
            });
        }
        if snapshot.open_tool_count > 0 {
            return Ok(Judgement {
                state: SemanticState::Working,
                reason: None,
                confidence: high,
            });
        }
        if snapshot.turn_stopped && snapshot.execution_active {
            return Ok(Judgement {
                state: SemanticState::Waiting,
                reason: Some(WaitReason::IdleTurnStop),
                confidence: high,
            });
        }
        if snapshot.execution_active {
            return Ok(Judgement {
                state: SemanticState::Working,
                reason: None,
                confidence: high,
            });
        }
        Ok(Judgement {
            state: SemanticState::Unknown,
            reason: Some(WaitReason::Other {
                detail: "no_execution_state".to_string(),
            }),
            confidence: high,
        })
    }

    fn name(&self) -> &'static str {
        "heuristic"
    }
}
