//! Reading a model answer against the facts: narrow it to the states the
//! facts allow, decide whether it needs a look, and fold COMPLETED into
//! WAITING for a session that never ends.

use super::judge::Judgement;
use super::{CONFIDENT, LIKELY};
use crate::observation::{Ambiguity, Confidence, SemanticState};

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
