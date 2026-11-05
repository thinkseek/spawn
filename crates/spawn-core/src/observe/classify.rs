//! Reading facts against answers. The rules tier decides what the facts
//! settle and marks the rest ambiguous; a model answer to an ambiguous case
//! is narrowed to the states the same facts allow, checked for whether it
//! needs a look, and, for a session that never ends, has COMPLETED folded
//! into WAITING.

use std::time::Duration;

use super::judge::{JudgeInput, Judgement, TurnFact};
use super::{CONFIDENT, LIKELY, LONG_TOOL, QUIET_AFTER};
use crate::event::EndCause;
use crate::observation::{Ambiguity, Confidence, SemanticState, WaitReason};
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
    // Work that exists only in a mounted workspace, which nothing can
    // commit: someone has to look, whatever else the facts say.
    if input.stalled_end.is_some() {
        return RulesVerdict::Certain {
            state: SemanticState::Blocked,
        };
    }
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
    // A tool open this long with no progress of the engine may hang,
    // however busy the screen looks: the spinner over a hung tool keeps
    // being redrawn, and read again. Evidence stored before progress was
    // kept falls back on the latest fact of any kind.
    let stalled_for = |limit: Duration| {
        input
            .progress_ms
            .or(input.quiet_ms)
            .is_some_and(|stalled| Duration::from_millis(stalled) >= limit)
    };
    if input.open_tools > 0 && stalled_for(LONG_TOOL) {
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

/// Index of `state` in [`SemanticState::ALL`], the order of a distribution.
fn index_of(state: SemanticState) -> Option<usize> {
    SemanticState::ALL
        .iter()
        .position(|candidate| *candidate == state)
}

/// Probability mass a distribution puts on `state`; negative mass counts as
/// none.
fn weight(probabilities: &[f64; 5], state: SemanticState) -> f64 {
    index_of(state).map_or(0.0, |index| probabilities[index].max(0.0))
}

/// Narrow a model answer to the states the facts allow.
///
/// With a distribution, the allowed states are renormalized and the most
/// likely one wins. Without one, a disallowed answer falls back to the rules'
/// guess with zero confidence, which asks for attention.
pub fn narrow(
    ambiguity: Ambiguity,
    guess: SemanticState,
    judgement: &Judgement,
) -> (SemanticState, Confidence) {
    let allowed = ambiguity.allowed();
    let Some(probabilities) = &judgement.probabilities else {
        return if allowed.contains(&judgement.state) {
            (judgement.state, judgement.confidence)
        } else {
            (guess, Confidence::ZERO)
        };
    };
    let weight = |state: SemanticState| weight(probabilities, state);
    let total: f64 = allowed.iter().map(|state| weight(*state)).sum();
    // Written as `> 0.0` so a NaN total also falls back to the guess.
    if total > 0.0 {
        // On a tie the rules' guess wins: it is the documented fallback.
        let best = allowed
            .iter()
            .copied()
            .max_by(|left, right| {
                weight(*left)
                    .total_cmp(&weight(*right))
                    .then_with(|| (*left == guess).cmp(&(*right == guess)))
            })
            .unwrap_or(guess);
        let confidence = Confidence::new(weight(best) / total).unwrap_or(Confidence::ZERO);
        return (best, confidence);
    }
    (guess, Confidence::ZERO)
}

/// Why an observation should draw a person's or a higher tier's eye, in one
/// word, or `None` when it should not.
///
/// What matters is the chance that someone is needed, not how sure the model
/// is about which harmless state applies: WAITING 0.56 against COMPLETED 0.40
/// is an idle agent either way and must not page anyone. With a distribution,
/// the mass on BLOCKED and UNKNOWN among the states the facts allow decides;
/// without one, a low confidence does. When most of the model's mass sits on
/// states the facts rule out, the model and the facts disagree, which is
/// itself worth a look.
pub fn attention_reason(
    ambiguity: Option<Ambiguity>,
    state: SemanticState,
    confidence: Confidence,
    judgement: &Judgement,
) -> Option<&'static str> {
    match state {
        SemanticState::Blocked => return Some("blocked"),
        SemanticState::Unknown => return Some("unknown"),
        _ => {}
    }
    if judgement.needs_human.is_some_and(|value| value >= LIKELY) {
        return Some("needs_human");
    }
    if judgement.stuck.is_some_and(|value| value >= LIKELY) {
        return Some("stuck");
    }
    let Some(probabilities) = &judgement.probabilities else {
        return (confidence.value() < CONFIDENT).then_some("low_confidence");
    };
    let allowed = ambiguity.map_or(&SemanticState::ALL[..], Ambiguity::allowed);
    let weight = |state: &SemanticState| weight(probabilities, *state);
    let total: f64 = allowed.iter().map(weight).sum();
    let everything: f64 = SemanticState::ALL.iter().map(weight).sum();
    if total <= 0.0 || total < everything * LIKELY {
        return Some("disagreement");
    }
    let risky: f64 = allowed
        .iter()
        .filter(|state| matches!(state, SemanticState::Blocked | SemanticState::Unknown))
        .map(weight)
        .sum();
    (risky / total >= LIKELY).then_some("risky")
}

/// For a session that never ends (the supervisor), COMPLETED only means it
/// waits for its next instruction: fold that mass into WAITING.
pub(super) fn fold_completed(judgement: &Judgement) -> Judgement {
    let index = |state: SemanticState| index_of(state).unwrap_or(0);
    let mut folded = *judgement;
    if let Some(probabilities) = &mut folded.probabilities {
        let (waiting, completed) = (
            index(SemanticState::Waiting),
            index(SemanticState::Completed),
        );
        probabilities[waiting] += probabilities[completed];
        probabilities[completed] = 0.0;
    }
    if folded.state == SemanticState::Completed {
        folded.state = SemanticState::Waiting;
    }
    folded
}
