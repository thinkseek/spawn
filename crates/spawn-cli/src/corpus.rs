//! Experience corpus: what the model judged, and what happened next.
//!
//! `spawn corpus export` writes one JSON line per model judgement: the exact
//! input the model saw (the observation's evidence), its answer, and an
//! outcome label derived from the log afterwards. `spawn judge --replay`
//! runs the current [`rules`] over such a file and reports which tier would
//! decide each episode and how often the rules agree with the model and with
//! the outcome. That is the measure of the rules-coverage objective: move
//! episodes from the model to the rules without losing agreement.

use std::collections::BTreeMap;
use std::io::BufRead;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spawn_core::observation::SemanticState;
use spawn_core::observe::{JudgeInput, RulesVerdict, rules};
use spawn_postgres::PostgresStore;

/// One corpus record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) event_id: i64,
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) recorded_at_ms: i64,
    /// What the model was shown.
    pub(crate) input: JudgeInput,
    /// What it answered, after narrowing.
    pub(crate) model: Value,
    /// What happened next.
    pub(crate) outcome: Outcome,
}

/// What happened after a judgement, read from the log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Outcome {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) next_principal: Option<String>,
    #[serde(default)]
    pub(crate) next_action: Option<String>,
    #[serde(default)]
    pub(crate) next_command_at_ms: Option<i64>,
    /// The next command was already on its way at the judged evidence.
    #[serde(default)]
    pub(crate) next_in_flight: bool,
    #[serde(default)]
    pub(crate) agent_moved_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) ended_at_ms: Option<i64>,
}

/// The outcome label: the agent went on by itself, someone had to act (and
/// who), it was stopped or restarted, it ended, or nothing happened yet.
pub(crate) fn label(outcome: &Outcome, supervisor: bool) -> &'static str {
    if outcome.agent_moved_at_ms.is_some() {
        return "continued";
    }
    // Input on its way at the judged moment (the first prompt after the
    // ready screen): the agent was waiting for it.
    if outcome.next_in_flight
        && matches!(outcome.next_action.as_deref(), Some("start" | "input" | "resume"))
    {
        return "needed_input";
    }
    match (outcome.next_action.as_deref(), outcome.next_principal.as_deref()) {
        (Some("input" | "resume"), Some("human" | "spawn_supervisor")) => "needed_decision",
        // The dispatcher types `continue` into workers; into the supervisor
        // it types digests, which are new input, not a nudge.
        (Some("input"), Some("spawn-dispatch")) if !supervisor => "nudged",
        (Some("resume"), Some("spawn-liveness" | "spawn-dispatch")) | (Some("start"), _) => {
            "restarted"
        }
        (Some("input" | "resume"), _) => "needed_input",
        (Some("stop"), _) => "stopped",
        _ if outcome.ended_at_ms.is_some() => "ended",
        _ => "open",
    }
}

/// States an outcome is consistent with. Empty: the outcome says nothing
/// about the state at judgement time (a restart follows an outage, which no
/// judgement could foresee; an open episode has no outcome yet).
pub(crate) fn consistent(label: &str) -> &'static [SemanticState] {
    use SemanticState::{Blocked, Completed, Waiting, Working};
    match label {
        "continued" => &[Working],
        "needed_decision" | "needed_input" => &[Waiting, Blocked],
        "nudged" => &[Waiting],
        "stopped" => &[Waiting, Completed],
        "ended" => &[Completed],
        _ => &[],
    }
}

/// Export every model judgement since `since_ms`, one JSON line each.
pub(crate) fn export(store: &PostgresStore, since_ms: i64) -> Result<Vec<Record>, String> {
    let rows = store
        .corpus_rows(since_ms)
        .map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for row in rows.as_array().cloned().unwrap_or_default() {
        let observation = row.get("observation").cloned().unwrap_or(Value::Null);
        let Some(evidence) = observation.get("evidence").cloned() else {
            continue;
        };
        let Ok(input) = serde_json::from_value::<JudgeInput>(evidence) else {
            continue;
        };
        let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
        let number = |key: &str| row.get(key).and_then(Value::as_i64);
        let mut outcome = Outcome {
            label: String::new(),
            next_principal: text("next_principal"),
            next_action: text("next_action"),
            next_command_at_ms: number("next_command_at_ms"),
            next_in_flight: row
                .get("next_in_flight")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            agent_moved_at_ms: number("agent_moved_at_ms"),
            ended_at_ms: number("ended_at_ms"),
        };
        let supervisor = text("agent_id").as_deref() == Some(crate::supervise::SUPERVISOR_AGENT);
        outcome.label = label(&outcome, supervisor).to_string();
        let mut model = observation;
        if let Some(object) = model.as_object_mut() {
            object.remove("evidence");
        }
        records.push(Record {
            event_id: number("event_id").unwrap_or(0),
            session_id: text("session_id").unwrap_or_default(),
            agent_id: text("agent_id").unwrap_or_default(),
            recorded_at_ms: number("recorded_at_ms").unwrap_or(0),
            input,
            model,
            outcome,
        });
    }
    Ok(records)
}

/// Read a corpus file, one record per line; blank lines are skipped.
pub(crate) fn read(path: &str) -> Result<Vec<Record>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("open {path}: {error}"))?;
    let mut records = Vec::new();
    for (number, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| format!("read {path}: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        records.push(
            serde_json::from_str(&line)
                .map_err(|error| format!("{path}:{}: {error}", number + 1))?,
        );
    }
    Ok(records)
}

fn model_state(record: &Record) -> Option<SemanticState> {
    record
        .model
        .get("state")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Replay the current rules over a corpus.
pub(crate) fn replay(records: &[Record]) -> Value {
    let mut by_tier: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_ambiguity: BTreeMap<String, u64> = BTreeMap::new();
    let (mut rules_agree_model, mut rules_labelled, mut rules_agree_outcome) = (0u64, 0u64, 0u64);
    let (mut model_labelled, mut model_agree_outcome) = (0u64, 0u64);
    for record in records {
        let mut input = record.input.clone();
        input.ambiguity = None;
        input.screen_tail = None;
        input.task = None;
        let consistent_states = consistent(&record.outcome.label);
        let model = model_state(record);
        if !consistent_states.is_empty() {
            model_labelled += 1;
            if model.is_some_and(|state| consistent_states.contains(&state)) {
                model_agree_outcome += 1;
            }
        }
        match rules(&input) {
            RulesVerdict::Certain { state } => {
                *by_tier.entry("rules").or_default() += 1;
                if model == Some(state) {
                    rules_agree_model += 1;
                }
                if !consistent_states.is_empty() {
                    rules_labelled += 1;
                    if consistent_states.contains(&state) {
                        rules_agree_outcome += 1;
                    }
                }
            }
            RulesVerdict::Ambiguous { ambiguity, .. } => {
                *by_tier.entry("model").or_default() += 1;
                *by_ambiguity
                    .entry(serde_json::to_value(ambiguity).map_or_else(
                        |_| format!("{ambiguity:?}"),
                        |value| value.as_str().unwrap_or_default().to_string(),
                    ))
                    .or_default() += 1;
            }
        }
    }
    let rules_decided = by_tier.get("rules").copied().unwrap_or(0);
    let ratio = |part: u64, whole: u64| {
        let float = |count: u64| f64::from(u32::try_from(count).unwrap_or(u32::MAX));
        if whole == 0 {
            Value::Null
        } else {
            json!(float(part) / float(whole))
        }
    };
    let total = u64::try_from(records.len()).unwrap_or(u64::MAX);
    json!({
        "total": records.len(),
        "by_tier": by_tier,
        "coverage": ratio(rules_decided, total),
        "still_ambiguous": by_ambiguity,
        "rules_agree_model": ratio(rules_agree_model, rules_decided),
        "rules_agree_outcome": ratio(rules_agree_outcome, rules_labelled),
        "rules_labelled": rules_labelled,
        "model_agree_outcome": ratio(model_agree_outcome, model_labelled),
        "model_labelled": model_labelled,
    })
}

#[cfg(test)]
mod tests {
    use super::{Outcome, label};

    fn label_worker(outcome: &Outcome) -> &'static str {
        label(outcome, false)
    }

    fn outcome(action: Option<&str>, principal: Option<&str>) -> Outcome {
        Outcome {
            next_action: action.map(str::to_string),
            next_principal: principal.map(str::to_string),
            ..Outcome::default()
        }
    }

    #[test]
    fn outcomes_are_labelled_by_who_acted_next() {
        assert_eq!(
            label_worker(&Outcome {
                agent_moved_at_ms: Some(1),
                ..outcome(Some("input"), Some("human"))
            }),
            "continued",
            "the agent moving first wins"
        );
        assert_eq!(label_worker(&outcome(Some("input"), Some("human"))), "needed_decision");
        assert_eq!(
            label_worker(&outcome(Some("input"), Some("spawn_supervisor"))),
            "needed_decision"
        );
        assert_eq!(label_worker(&outcome(Some("input"), Some("spawn-dispatch"))), "nudged");
        assert_eq!(label_worker(&outcome(Some("resume"), Some("spawn-liveness"))), "restarted");
        assert_eq!(label_worker(&outcome(Some("stop"), Some("spawn_supervisor"))), "stopped");
        assert_eq!(label_worker(&outcome(None, None)), "open");
        assert_eq!(
            label(&outcome(Some("input"), Some("spawn-dispatch")), true),
            "needed_input",
            "a digest is input, not a nudge"
        );
        assert_eq!(
            label_worker(&Outcome {
                ended_at_ms: Some(5),
                ..outcome(None, None)
            }),
            "ended"
        );
    }
}
