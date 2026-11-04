//! The rules tier: decide what the facts settle, mark the rest ambiguous.

use std::time::Duration;

use super::judge::{JudgeInput, TurnFact};
use super::{LONG_TOOL, QUIET_AFTER};
use crate::event::EndCause;
use crate::observation::{Ambiguity, SemanticState, WaitReason};
use crate::screen::ScreenState;

/// What the rules tier concluded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RulesVerdict {
    /// The facts settle the state.
    Certain {
        /// Decided state.
        state: SemanticState,
    },
    /// The facts leave several states open.
    Ambiguous {
        /// Why.
        ambiguity: Ambiguity,
        /// The rules' best guess, used when no model can answer.
        guess: SemanticState,
    },
}

/// The rules tier: decide what the facts settle, mark the rest ambiguous.
pub fn rules(input: &JudgeInput) -> RulesVerdict {
    if input.execution_active {
        live_execution(input)
    } else {
        ended_execution(input)
    }
}

/// Rules for a session whose execution is running.
fn live_execution(input: &JudgeInput) -> RulesVerdict {
    use RulesVerdict::{Ambiguous, Certain};
    // A permission dialog is a fact only while its pane exists.
    if input.permission_pending || input.screen == Some(ScreenState::Permission) {
        return Certain {
            state: SemanticState::Blocked,
        };
    }
    let quiet_for = |limit: Duration| {
        input
            .quiet_ms
            .is_some_and(|quiet| Duration::from_millis(quiet) >= limit)
    };
    if input.open_tools > 0 && quiet_for(LONG_TOOL) {
        return Ambiguous {
            ambiguity: Ambiguity::QuietLive,
            guess: SemanticState::Working,
        };
    }
    if input.open_tools > 0 || input.screen == Some(ScreenState::Busy) {
        return Certain {
            state: SemanticState::Working,
        };
    }
    if input.turn == Some(TurnFact::Failed) || input.screen == Some(ScreenState::Error) {
        return Ambiguous {
            ambiguity: Ambiguity::EngineError,
            guess: SemanticState::Working,
        };
    }
    if input.turn == Some(TurnFact::Stopped) || input.screen == Some(ScreenState::Ready) {
        return Ambiguous {
            ambiguity: Ambiguity::TurnStoppedIdle,
            guess: SemanticState::Waiting,
        };
    }
    if quiet_for(QUIET_AFTER) {
        return Ambiguous {
            ambiguity: Ambiguity::QuietLive,
            guess: SemanticState::Working,
        };
    }
    // A turn is running and the latest fact is recent.
    Certain {
        state: SemanticState::Working,
    }
}

/// Rules for a session with no running execution.
fn ended_execution(input: &JudgeInput) -> RulesVerdict {
    use RulesVerdict::{Ambiguous, Certain};
    match input.last_end {
        Some(EndCause::Stopped) => Certain {
            state: SemanticState::Completed,
        },
        // A failing exit status makes "it got stuck" the safer guess.
        Some(EndCause::Exited) => Ambiguous {
            ambiguity: Ambiguity::ExitedOnItsOwn,
            guess: if exited_failing(input) {
                SemanticState::Blocked
            } else {
                SemanticState::Completed
            },
        },
        Some(EndCause::Lost) => Ambiguous {
            ambiguity: Ambiguity::Lost,
            guess: SemanticState::Unknown,
        },
        None => Certain {
            state: SemanticState::Waiting,
        },
    }
}

/// Whether the latest execution reported a failing exit status.
pub(super) fn exited_failing(input: &JudgeInput) -> bool {
    input.exit_code.is_some_and(|code| code != 0)
}

/// Why a session in `state` waits, as far as the facts tell.
pub(super) fn wait_reason(state: SemanticState, input: &JudgeInput) -> Option<WaitReason> {
    match state {
        SemanticState::Blocked if input.permission_pending => {
            Some(WaitReason::WaitingForUserPermission)
        }
        SemanticState::Waiting => Some(WaitReason::WaitingForUserInput),
        _ => None,
    }
}
