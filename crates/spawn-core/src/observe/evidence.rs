//! What the model judge is shown beyond the log, and how a repeated question
//! is recognised.

use super::judge::JudgeInput;
use crate::id::SessionId;

/// Characters of terminal tail sent to the model and kept as evidence.
const SCREEN_CHARS: usize = 2500;

/// Characters of the task sent to the model and kept as evidence.
const TASK_CHARS: usize = 1000;

/// Characters of a `HUMAN:` question kept on the observation.
const QUESTION_CHARS: usize = 300;

/// Context the model judge needs and the log does not hold.
pub trait ObservationContext: Send + Sync {
    /// Tail of the session's terminal, if one runs.
    fn screen_tail(&self, session: &SessionId) -> Option<String>;

    /// The latest task typed into the session.
    fn task(&self, session: &SessionId) -> Option<String>;

    /// Whether the session is meant never to end (the supervisor). Its
    /// COMPLETED is read as WAITING.
    fn perpetual(&self, _session: &SessionId) -> bool {
        false
    }
}

/// Fill in what the context knows about `session`: the terminal tail and the
/// task, truncated, and whether the session never ends. Without a context
/// they stay empty.
pub(super) fn attach_context(
    input: &mut JudgeInput,
    context: Option<&dyn ObservationContext>,
    session: &SessionId,
) {
    let Some(context) = context else {
        input.perpetual = false;
        return;
    };
    input.screen_tail = context
        .screen_tail(session)
        .map(|text| tail_chars(text.trim_end(), SCREEN_CHARS));
    input.task = context
        .task(session)
        .map(|text| head_chars(&text, TASK_CHARS));
    input.perpetual = context.perpetual(session);
}

/// Whether the screen ends with a question for a person: a line that starts
/// with `HUMAN:` (after an optional `claude:` speaker tag), below the last
/// line the user typed. An answered question no longer counts.
pub fn asks_human(screen_tail: &str) -> bool {
    human_question(screen_tail).is_some()
}

/// The last unanswered `HUMAN:` question on the screen, without the tag.
pub fn human_question(screen_tail: &str) -> Option<String> {
    let lines: Vec<&str> = screen_tail.lines().collect();
    let start = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("you:"))
        .map_or(0, |index| index + 1);
    lines[start..].iter().rev().find_map(|line| {
        let text = line.trim_start();
        let text = text.strip_prefix("claude:").map_or(text, str::trim_start);
        text.strip_prefix("HUMAN:")
            .map(|question| head_chars(question.trim(), QUESTION_CHARS))
    })
}

/// Stable digest of what the model is shown (FNV-1a over its JSON), so a
/// stored fingerprint still matches after a restart or a rebuild.
pub(super) fn fingerprint_of(input: &JudgeInput) -> String {
    let shown = serde_json::json!([
        input.ambiguity,
        input.evidence_seq.0,
        input.exit_code,
        input.screen_tail,
        input.task,
        input.perpetual,
    ]);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in shown.to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The last `limit` characters of `text`.
fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

/// The first `limit` characters of `text`.
fn head_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::asks_human;

    #[test]
    fn a_question_for_a_person_is_seen_until_answered() {
        assert!(asks_human(
            "you: deploy it\nclaude: Done with the build.\nclaude: HUMAN: which region?\n$ "
        ));
        assert!(asks_human(
            "claude: The key is missing.\nHUMAN: please add it\n"
        ));
        assert!(!asks_human(
            "claude: HUMAN: which region?\nyou: eu-west\nclaude: deploying\n"
        ));
        assert!(!asks_human(
            "claude: a human should look at the HUMAN: tag docs\n"
        ));
    }
}
