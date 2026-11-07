//! `GET /api/attention`: the UI's "Escalations" queue, computed by the
//! dispatcher's own code.
//!
//! The escalations are the plan the dispatcher would make this moment,
//! from the same reads and the same functions as its pass
//! ([`dispatch::read_plan`]: the facts and mission rows, the disk and
//! gate-room records, the stalls, then [`dispatch::decide_with`]). Nothing
//! is submitted, marked or collected: no report is collected, no delivery
//! marked, no command sent. What the supervisor was already told is read
//! from the same told set the dispatcher filters on.
//!
//! The answer:
//!
//! ```text
//! {
//!   "generated_at_ms": 1793957840898,
//!   "pending": [{                  // the plan's escalations not told yet, in its order
//!     "session_id": "session_worker_a" | "host" | "missions",
//!     "agent_id": "agent_worker_a" | null,
//!     "label": "...",              // what it is doing (serve/labels.rs), with
//!     "task_title": "..." | null,  // its task's title and mission's name
//!     "mission_name": "..." | null,
//!     "why": "stalled",            // the digest's heading
//!     "line": "...", "tag": "session_worker_a stall-7-activity-41",
//!     "since_ms": 1793957000000 | null,  // when the record it rests on was written
//!     "told": false, "told_at_ms": null
//!   }],
//!   "told": [ ...the same, "told": true, "told_at_ms": <first digest that carried it> | null ],
//!   "holds": [{                    // the owner's holds with a task waiting behind them
//!     "source", "hold", "main", "by", "reason", "held_at_ms", "expires_ms",
//!     "waiting": [12]              // the tasks waiting, oldest first
//!   }],
//!   "supervisor": null | {
//!     "session_id", "agent_id", "label", "live", "execution_id", "end_cause",
//!     "busy",                      // a command into it is in flight
//!     "turn_ended",                // its live execution's latest turn has ended
//!     "judgement": null | {state, needs_human, stuck, ambiguity, confidence, judge,
//!                          attention, attention_reason, judged_at_ms, current},
//!     "question": null | {
//!       "text": "...",             // its last reply (the latest turn's own words)
//!       "source": "transcript" | "screen" | null,
//!       "asked_at_ms": 1793957000000 | null,  // the end of that turn
//!       "human": null | {"text": "Should I ...?", "at_ms": ... | null}
//!     },
//!     "for_owner": [{"text": "deploy ffc9fe7", "at_ms": 1793957000000 | null}]
//!   },
//!   "on_own_jobs": [{              // workers the rules hold as working on a
//!     "session_id", "agent_id",    // background job of their own: not escalated
//!     "label", "task_title", "mission_name",
//!     "judged_at_ms": 1793957000000 | null,
//!     "jobs": [{"id": "b8w03og7k", "kind": "bash" | "monitor",
//!               "started_at_ms": ..., "age_ms": ...}]   // oldest first
//!   }],
//!   "warnings": ["dispatch: stalls: ..."]   // what could not be read
//! }
//! ```
//!
//! `on_own_jobs` lists the workers whose current judgement is the rules'
//! "waiting on its own background job" ([`Facts::waits_on_own_job`]), with
//! their open jobs and each one's age on the database's clock: the reason
//! they are in no queue. A job past its bound is an item of its own in
//! `pending` or `told` (`background job past its bound`).
//!
//! `holds` is not among `pending` or `told`: the supervisor is told of a
//! hold's expiry, never of a hold. It lists the owner's active holds on
//! mission sources (`spawn mission hold`) that have at least one task waiting
//! behind them, as the mission board shows them: work a person is holding.
//!
//! The supervisor's question is shown when its turn ended at its prompt
//! and its judgement asks for a person: BLOCKED or WAITING with
//! `needs_human` at least [`ASKS_NEEDS_HUMAN`], or a `HUMAN:` line the
//! Observer read in its reply. Its words come from its transcript (the
//! engine's own record, by role), read with the Observer's reader
//! ([`Reply`]) over the transcript's end; the log names the transcript
//! (`session.native_started`). When the transcript cannot be read, the
//! screen tail stored with the judgement stands in. `human` is the last
//! `HUMAN:` line it wrote since a prompt that was not a dispatcher digest:
//! a digest answers no question, so one asked a few turns ago is still open.
//!
//! A `For the owner:` line is no question. The supervisor's charter
//! (`supervise::charter`) has it decide what its grants allow and name what
//! they do not (a deploy, an owner-protected file, money, deleting data, a
//! grant) in such a line, then move on without waiting. A latest reply with
//! one asks nothing, whatever the judge made of it, unless a `HUMAN:`
//! question is open (an old-style turn, answered by no person since).
//! Instead `for_owner` lists the lines, read from the same transcript end
//! whenever there is a supervisor: newest first, each once (its latest
//! time), at most [`FOR_OWNER_ITEMS`]. They are items for the owner to read,
//! not a queue: nothing waits on them, and an owner's message, an
//! instruction rather than an answer, does not clear them. Without a
//! transcript the list is empty and the judgement alone decides.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use spawn_core::id::WorkspaceId;
use spawn_core::observation::{Ambiguity, Observation, SemanticState};
use spawn_core::observe::reply::{Reply, ReplyPart, claude_reply};
use spawn_core::port::TerminalHost;

use crate::dispatch::{self, Escalation, Facts, Plan};
use crate::supervise::SUPERVISOR_SESSION;

/// Least `needs_human` of a supervisor's judgement whose question is shown.
const ASKS_NEEDS_HUMAN: f64 = 0.5;

/// Bytes of the transcript's end read for the supervisor's words: the end
/// of its latest reply, and the questions of its last turns.
const TRANSCRIPT_TAIL: u64 = 1 << 20;

/// What starts a line the supervisor writes for the owner without waiting
/// on it (its charter's `For the owner:`), the colon after it.
const FOR_OWNER: &str = "For the owner";

/// Most `For the owner:` lines shown.
const FOR_OWNER_ITEMS: usize = 10;

/// `GET /api/attention`.
pub(super) fn attention() -> Result<Value, String> {
    let store = super::open_store()?;
    let disk = store.host_disk().map_err(super::err)?;
    let (bounds, _) = dispatch::Bounds::from_env();
    let reading = dispatch::read_plan(
        &store,
        dispatch::load(&store)?,
        |now| crate::disk::escalations(disk.as_array().map_or(&[][..], Vec::as_slice), now),
        &bounds,
        crate::config::supervised(),
        &crate::config::spawn_program(),
        // It submits nothing: no charter input to plan.
        None,
    )?;
    let mut warnings = reading.lines;
    let tags: Vec<String> = reading.plan.told.iter().map(Escalation::tag).collect();
    let told_at = if tags.is_empty() {
        Value::Null
    } else {
        store
            .told_at(SUPERVISOR_SESSION, &tags)
            .unwrap_or_else(|error| {
                warnings.push(format!("attention: when told: {error}"));
                Value::Null
            })
    };
    let supervisor = reading
        .rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION);
    // Its words, whenever there is a supervisor: its lines for the owner
    // are worth reading while it works, and they decide whether it asks.
    let (turn, said) = match supervisor.map(|_| store.latest_turn(SUPERVISOR_SESSION)) {
        Some(Ok(turn)) => {
            let said = transcript_tail(&turn).map(|bytes| said(&bytes));
            (turn, said)
        }
        Some(Err(error)) => {
            warnings.push(format!("attention: the supervisor's turn: {error}"));
            (Value::Null, None)
        }
        None => (Value::Null, None),
    };
    let question = supervisor
        .and_then(|facts| {
            asks(facts, said.as_ref())
                .map(|observation| question(facts, observation, &turn, said.as_ref()))
        })
        .unwrap_or(Value::Null);
    let for_owner = said
        .as_ref()
        .map_or(&[][..], |said| said.for_owner.as_slice())
        .iter()
        .map(|(text, at_ms)| json!({ "text": text, "at_ms": at_ms }))
        .collect();
    let holds = match store.source_holds() {
        Ok(holds) => waiting_holds(&holds),
        Err(error) => {
            warnings.push(format!("attention: the owner's holds: {error}"));
            Value::Array(Vec::new())
        }
    };
    let mut view = answer(
        &reading.rows,
        &reading.plan,
        &told_at,
        &question,
        &Value::Array(for_owner),
        &warnings,
        super::now_ms(),
    );
    if let Some(fields) = view.as_object_mut() {
        fields.insert("holds".to_string(), holds);
    }
    // Labels name the agents (serve/labels.rs); without their facts the
    // items are labelled by their ids alone, and the view still answers.
    let context = super::label_context(&store).unwrap_or_else(|error| {
        if let Some(warnings) = view.get_mut("warnings").and_then(Value::as_array_mut) {
            warnings.push(json!(format!("attention: labels: {error}")));
        }
        super::labels::Context::default()
    });
    super::labels::label_attention(&mut view, &context);
    Ok(view)
}

/// The owner's holds of `holds` (`PostgresStore::source_holds`: `{now_ms,
/// holds}`) that have at least one task waiting behind them.
fn waiting_holds(holds: &Value) -> Value {
    Value::Array(
        holds["holds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|hold| {
                hold["waiting"]
                    .as_array()
                    .is_some_and(|tasks| !tasks.is_empty())
            })
            .cloned()
            .collect(),
    )
}

/// The answer from what was read: `plan` as [`dispatch::decide_with`] made
/// it from `rows`, when each told item was first told (`told_at`, `{tag:
/// ms}`), the supervisor's `question` ([`question`], or null), its lines
/// for the owner (`for_owner`, `[{text, at_ms}]`) and the `warnings` of the
/// reads.
fn answer(
    rows: &[Facts],
    plan: &Plan,
    told_at: &Value,
    question: &Value,
    for_owner: &Value,
    warnings: &[String],
    generated_at_ms: i64,
) -> Value {
    let agents: BTreeMap<&str, &str> = rows
        .iter()
        .map(|facts| (facts.session_id.as_str(), facts.agent_id.as_str()))
        .collect();
    let item = |escalation: &Escalation, told: bool| {
        let tag = escalation.tag();
        json!({
            "session_id": escalation.session_id,
            "agent_id": agents.get(escalation.session_id.as_str()),
            "why": dispatch::heading(escalation.why),
            "line": escalation.line,
            "since_ms": escalation.since_ms,
            "told": told,
            "told_at_ms": told_at.get(&tag).and_then(Value::as_i64),
            "tag": tag,
        })
    };
    let supervisor = rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION)
        .map_or(Value::Null, |facts| supervisor(facts, question, for_owner));
    json!({
        "generated_at_ms": generated_at_ms,
        "pending": plan.pending.iter().map(|escalation| item(escalation, false)).collect::<Vec<_>>(),
        "told": plan.told.iter().map(|escalation| item(escalation, true)).collect::<Vec<_>>(),
        "supervisor": supervisor,
        "on_own_jobs": on_own_jobs(rows),
        "warnings": warnings,
    })
}

/// The workers judged, on their latest fact, at their prompt waiting on a
/// background job of their own, with those jobs: working, so in no queue.
fn on_own_jobs(rows: &[Facts]) -> Vec<Value> {
    rows.iter()
        .filter(|facts| facts.session_id != SUPERVISOR_SESSION && facts.waits_on_own_job())
        .map(|facts| {
            json!({
                "session_id": facts.session_id,
                "agent_id": facts.agent_id,
                "judged_at_ms": facts.observation_at_ms,
                "jobs": facts.jobs.iter().map(|job| json!({
                    "id": job.id,
                    "kind": job.kind,
                    "started_at_ms": job.started_at_ms,
                    "age_ms": job.age_ms,
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The supervisor's liveness, its latest judgement, its `question` and its
/// lines `for_owner`.
fn supervisor(facts: &Facts, question: &Value, for_owner: &Value) -> Value {
    let judgement = facts.observation.as_ref().map(|observation| {
        json!({
            "state": observation.state.key(),
            "needs_human": observation.needs_human,
            "stuck": observation.stuck,
            "ambiguity": observation.ambiguity,
            "confidence": observation.confidence.value(),
            "judge": observation.judge,
            "attention": observation.attention,
            "attention_reason": observation.attention_reason,
            "judged_at_ms": facts.observation_at_ms,
            // Not older than the session's latest fact (the dispatcher acts
            // only on a current one).
            "current": facts.current_observation().is_some(),
        })
    });
    json!({
        "session_id": facts.session_id,
        "agent_id": facts.agent_id,
        "live": facts.live,
        "execution_id": facts.execution_id,
        "end_cause": facts.end_cause,
        "busy": facts.open_command,
        "turn_ended": facts.turn_ended,
        "judgement": judgement,
        "question": question,
        "for_owner": for_owner,
    })
}

/// The supervisor's latest judgement, when it asks a person something: its
/// live execution's latest turn has ended at its prompt (the judgement
/// says the turn stopped idle), and it is judged BLOCKED or WAITING with
/// `needs_human` at least [`ASKS_NEEDS_HUMAN`], or the Observer read a
/// `HUMAN:` line in its reply. The latest judgement, current or not: a
/// screen reading after it changes nothing the turn's end does not.
fn asking(facts: &Facts) -> Option<&Observation> {
    let observation = facts.observation.as_ref()?;
    let at_prompt =
        facts.live && facts.turn_ended && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle);
    let unsure = matches!(
        observation.state,
        SemanticState::Blocked | SemanticState::Waiting
    ) && observation
        .needs_human
        .is_some_and(|value| value >= ASKS_NEEDS_HUMAN);
    (at_prompt && (unsure || observation.human_question.is_some())).then_some(observation)
}

/// The supervisor's judgement when it asks a person something ([`asking`]),
/// unless its latest reply in its transcript (`said`, none when it could
/// not be read) has a [`FOR_OWNER`] line and no `HUMAN:` question is open:
/// such a line is one it does not wait on, whatever the judge made of it.
fn asks<'a>(facts: &'a Facts, said: Option<&Said>) -> Option<&'a Observation> {
    let observation = asking(facts)?;
    let open =
        observation.human_question.is_some() || said.is_some_and(|said| said.human.is_some());
    let names_owner = said
        .and_then(|said| said.reply.as_deref())
        .is_some_and(|reply| reply.lines().any(|line| for_owner(line).is_some()));
    (open || !names_owner).then_some(observation)
}

/// What the supervisor asks, from the end of its transcript (`said`, none
/// when it could not be read) or else the screen tail stored with its
/// judgement; asked when its latest turn ended (`turn`, from
/// `PostgresStore::latest_turn`), else when it was judged.
fn question(facts: &Facts, observation: &Observation, turn: &Value, said: Option<&Said>) -> Value {
    let asked_at = turn["turn_ended_at_ms"]
        .as_i64()
        .or(facts.observation_at_ms);
    let said = said.filter(|said| said.reply.is_some());
    let (text, source, human) = if let Some(said) = said {
        (said.reply.clone(), Some("transcript"), said.human.clone())
    } else {
        let screen = observation
            .evidence
            .as_ref()
            .and_then(|evidence| evidence["screen_tail"].as_str())
            .map(str::to_string);
        let source = screen.as_ref().map(|_| "screen");
        let human = observation
            .human_question
            .clone()
            .map(|question| (question, asked_at));
        (screen, source, human)
    };
    json!({
        "text": text,
        "source": source,
        "asked_at_ms": asked_at,
        "human": human.map(|(text, at_ms)| json!({ "text": text, "at_ms": at_ms })),
    })
}

/// The end of the supervisor's transcript the log names (`turn`, from
/// `PostgresStore::latest_turn`): its last [`TRANSCRIPT_TAIL`] bytes, read
/// through the terminal host the Observer reads it with. `None` when it
/// cannot be read here.
fn transcript_tail(turn: &Value) -> Option<Vec<u8>> {
    let path = turn["transcript"].as_str()?;
    let workspace = WorkspaceId::new(turn["workspace"].as_str()?).ok()?;
    let length = std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)?
        .len();
    crate::config::terminal_host()
        .read_from(&workspace, path, length.saturating_sub(TRANSCRIPT_TAIL))
        .ok()?
}

/// What the end of a transcript says the model wrote last.
#[derive(Debug, Default, PartialEq)]
struct Said {
    /// Its reply since the last prompt, as the Observer reads it ([`Reply`]).
    reply: Option<String>,
    /// The last `HUMAN:` line it wrote since a prompt that was not a
    /// dispatcher digest, and that line's time.
    human: Option<(String, Option<i64>)>,
    /// Its [`FOR_OWNER`] lines, whatever prompts came since: newest first,
    /// each once with its latest time, at most [`FOR_OWNER_ITEMS`].
    for_owner: Vec<(String, Option<i64>)>,
}

/// Read `bytes`, the end of a Claude Code transcript. A first line cut by
/// where the read started is not JSON and counts nothing.
fn said(bytes: &[u8]) -> Said {
    let mut reply = Reply::default();
    reply.feed(bytes);
    let whole = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(&[][..], |end| &bytes[..end]);
    let mut human = None;
    let mut notes = Vec::new();
    for line in whole.split(|byte| *byte == b'\n') {
        let line = String::from_utf8_lossy(line);
        match claude_reply(&line) {
            Some(ReplyPart::Prompt) if !is_digest(&line) => human = None,
            Some(ReplyPart::Text(text)) => {
                if let Some(question) = text
                    .lines()
                    .rev()
                    .find_map(|line| line.trim_start().strip_prefix("HUMAN:"))
                {
                    human = Some((question.trim().to_string(), line_time(&line)));
                }
                let words: Vec<&str> = text
                    .lines()
                    .filter_map(for_owner)
                    .filter(|words| !words.is_empty())
                    .collect();
                if !words.is_empty() {
                    let at = line_time(&line);
                    notes.extend(words.into_iter().map(|words| (words.to_string(), at)));
                }
            }
            _ => {}
        }
    }
    let mut for_owner: Vec<(String, Option<i64>)> = Vec::new();
    for (words, at) in notes.into_iter().rev() {
        if for_owner.len() == FOR_OWNER_ITEMS {
            break;
        }
        if !for_owner.iter().any(|(seen, _)| *seen == words) {
            for_owner.push((words, at));
        }
    }
    Said {
        reply: reply.text().map(str::to_string),
        human,
        for_owner,
    }
}

/// The words of a line the supervisor wrote for the owner: one that starts
/// with [`FOR_OWNER`] and a colon, after any list marker, quote or emphasis
/// (`- **For the owner:** ...`). Empty for a heading over a list.
fn for_owner(line: &str) -> Option<&str> {
    let emphasis = |c: char| matches!(c, '*' | '_');
    let rest = line
        .trim_start_matches(|c: char| c.is_whitespace() || emphasis(c) || matches!(c, '-' | '>'))
        .strip_prefix(FOR_OWNER)?
        .trim_start_matches(emphasis)
        .strip_prefix(':')?;
    Some(
        rest.trim_start_matches(|c: char| c.is_whitespace() || emphasis(c))
            .trim_end_matches(|c: char| c.is_whitespace() || c == '*'),
    )
}

/// Whether a transcript's prompt line is a dispatcher digest
/// ([`dispatch::DIGEST_OPENING`]).
fn is_digest(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let content = &value["message"]["content"];
    let text = content.as_str().or_else(|| {
        content
            .as_array()?
            .iter()
            .find_map(|block| block["text"].as_str())
    });
    text.is_some_and(|text| text.trim_start().starts_with(dispatch::DIGEST_OPENING))
}

/// A transcript line's `timestamp`, in epoch milliseconds.
fn line_time(line: &str) -> Option<i64> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    crate::scorecard::time::parse_rfc3339(value["timestamp"].as_str()?)
}

#[cfg(test)]
mod tests {
    use super::{FOR_OWNER_ITEMS, answer, asking, asks, for_owner, question, said, waiting_holds};
    use crate::deliver::Work;
    use crate::dispatch::{Escalation, Facts, Why, decide_with};
    use serde_json::{Value, json};

    const NOW: i64 = 1_793_957_840_000;

    /// One row as `dispatch_facts` returns it.
    fn facts(row: &Value) -> Facts {
        serde_json::from_value(row.clone()).expect("a facts row")
    }

    fn judged(state: &str, needs_human: f64, attention: bool, seq: u64) -> Value {
        json!({
            "state": state, "reason": null, "based_on_seq": seq, "confidence": 0.9,
            "judge": "jev", "ambiguity": "turn_stopped_idle", "needs_human": needs_human,
            "stuck": 0.1, "attention": attention,
            "evidence": {"screen_tail": "⏺ Should I stop objective_87 now?\n╭──╮\n│ > │"},
        })
    }

    /// A live worker or the supervisor, judged `observation` at `at`.
    fn row(session: &str, agent: &str, observation: &Value, at: i64) -> Value {
        json!({
            "session_id": session, "agent_id": agent, "live": true,
            "execution_id": format!("exec_{agent}"), "observation": observation,
            "observation_event_id": observation["based_on_seq"].as_i64().map(|seq| seq * 10),
            "observation_at_ms": at,
            "evidence_seq": observation["based_on_seq"], "turn_ended": true,
            "episode": 3, "open_command": false, "told": [],
        })
    }

    /// The live situation of 2026-11-06 in miniature. Worker 82's engine
    /// was lost and worker 85 is judged BLOCKED with attention, but both
    /// tasks are reported or accepted (a managed worker between tasks is not
    /// news). Worker 87, on a delivered task, asks for attention. Worker 90's
    /// resume failed, and a digest told the supervisor so. The supervisor
    /// sits at its prompt, BLOCKED, needs_human 0.87, and a disk record waits.
    fn situation() -> (Vec<Facts>, Work, Vec<Escalation>) {
        let lost = |session: &str, agent: &str, at: i64| {
            let mut lost = row(session, agent, &judged("UNKNOWN", 0.1, false, 4), at);
            lost["live"] = json!(false);
            lost["end_cause"] = json!("lost");
            lost["resumed_young"] = json!(1);
            lost
        };
        let mut supervisor = row(
            "session_supervisor",
            "agent_supervisor",
            &judged("BLOCKED", 0.87, false, 1351),
            NOW - 4 * 3_600_000,
        );
        supervisor["told"] = json!([
            "session_worker_objective_90 resume-exec_agent_worker_objective_90",
            "session_worker_gone resume-exec_1",
        ]);
        let rows = vec![
            facts(&lost(
                "session_worker_objective_82",
                "agent_worker_objective_82",
                NOW - 5 * 3_600_000,
            )),
            facts(&row(
                "session_worker_objective_85",
                "agent_worker_objective_85",
                &judged("BLOCKED", 0.9, true, 20),
                NOW - 60_000,
            )),
            facts(&row(
                "session_worker_objective_87",
                "agent_worker_objective_87",
                &judged("BLOCKED", 0.9, true, 30),
                NOW - 120_000,
            )),
            facts(&lost(
                "session_worker_objective_90",
                "agent_worker_objective_90",
                NOW - 3 * 3_600_000,
            )),
            facts(&supervisor),
        ];
        let task = |id: i64, state: &str| {
            json!({
                "id": id, "mission_id": 4, "kind": "ephemeral",
                "agent_id": format!("agent_worker_objective_{id}"),
                "session_id": format!("session_worker_objective_{id}"),
                "brief": "b", "state": state, "revisions": 0, "mission_text": "m",
            })
        };
        let work = Work {
            tasks: serde_json::from_value(json!([
                task(82, "accepted"),
                task(85, "reported"),
                task(87, "delivered"),
            ]))
            .expect("task rows"),
            ..Work::default()
        };
        let disk = crate::disk::escalations(
            &[json!({
                "event_id": 7, "at_ms": NOW - 600_000, "label": "tmpdir", "path": "/var/tmp",
                "state": "low", "free_bytes": 5_u64 << 30, "warn_bytes": 20_u64 << 30,
                "floor_bytes": 2_u64 << 30,
            })],
            NOW,
        );
        (rows, work, disk)
    }

    #[test]
    fn the_views_pending_is_the_dispatchers_pending() {
        let (rows, work, host) = situation();
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let view = answer(
            &rows,
            &plan,
            &Value::Null,
            &Value::Null,
            &json!([]),
            &[],
            NOW,
        );
        let pending = view["pending"].as_array().expect("pending");
        assert_eq!(pending.len(), plan.pending.len());
        for (shown, planned) in pending.iter().zip(&plan.pending) {
            assert_eq!(shown["tag"], planned.tag());
            assert_eq!(shown["session_id"], planned.session_id);
            assert_eq!(shown["why"], crate::dispatch::heading(planned.why));
            assert_eq!(shown["line"], planned.line);
            assert_eq!(shown["since_ms"], json!(planned.since_ms));
            assert_eq!(shown["told"], false);
        }
        let tags: Vec<&str> = pending
            .iter()
            .filter_map(|item| item["tag"].as_str())
            .collect();
        assert_eq!(
            tags,
            ["session_worker_objective_87 300", "host disk-7"],
            "workers 82 and 85 between tasks are not news; worker 90 was told"
        );
        assert_eq!(pending[0]["agent_id"], "agent_worker_objective_87");
        assert_eq!(pending[0]["why"], "attention");
        assert_eq!(pending[0]["since_ms"], NOW - 120_000);
        assert_eq!(pending[1]["agent_id"], Value::Null, "the host is no agent");
        assert_eq!(pending[1]["why"], "disk space on this host");
        assert_eq!(pending[1]["since_ms"], NOW - 600_000);
        assert_eq!(view["generated_at_ms"], NOW);
    }

    /// The live sequence of 2026-11-06 23:56: task 92's worker waited at
    /// its prompt on a gate it ran in the background. The rules hold it as
    /// working on that job: it is in no queue, and the view lists it with
    /// its job and the job's age. Past the job's bound, the job is an item.
    #[test]
    fn a_worker_on_its_own_background_job_is_listed_as_working_not_queued() {
        let (mut rows, mut work, host) = situation();
        let on_job = json!({
            "state": "WORKING", "reason": "WAITING_FOR_BACKGROUND_JOB", "based_on_seq": 50,
            "confidence": 1.0, "judge": "rules",
        });
        let mut waiting = row(
            "session_worker_gate_cache_92",
            "agent_worker_gate_cache_92",
            &on_job,
            NOW - 30_000,
        );
        waiting["jobs"] = json!([{"id": "bys3gpa46", "kind": "monitor", "event_id": 512,
                                  "started_at_ms": NOW - 900_000, "age_ms": 900_000}]);
        rows.push(facts(&waiting));
        work.tasks.extend(
            serde_json::from_value::<Vec<crate::deliver::TaskRow>>(json!([{
                "id": 92, "mission_id": 4, "kind": "ephemeral",
                "agent_id": "agent_worker_gate_cache_92",
                "session_id": "session_worker_gate_cache_92",
                "brief": "b", "state": "delivered", "revisions": 0, "mission_text": "m",
            }]))
            .expect("task rows"),
        );
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let view = answer(
            &rows,
            &plan,
            &Value::Null,
            &Value::Null,
            &json!([]),
            &[],
            NOW,
        );
        let queued = |view: &Value| -> Vec<String> {
            ["pending", "told"]
                .iter()
                .flat_map(|list| view[*list].as_array().expect("a list").clone())
                .filter_map(|item| item["session_id"].as_str().map(str::to_string))
                .collect()
        };
        assert!(
            !queued(&view).contains(&"session_worker_gate_cache_92".to_string()),
            "{view:#}"
        );
        assert_eq!(
            view["on_own_jobs"],
            json!([{
                "session_id": "session_worker_gate_cache_92",
                "agent_id": "agent_worker_gate_cache_92",
                "judged_at_ms": NOW - 30_000,
                "jobs": [{"id": "bys3gpa46", "kind": "monitor",
                          "started_at_ms": NOW - 900_000, "age_ms": 900_000}],
            }])
        );

        // Two hours on: the job is an item of its own, still not idle.
        let last = rows.len() - 1;
        rows[last].jobs[0].age_ms = 2 * 3_600_000 + 1;
        let mut host = host;
        host.extend(crate::dispatch::job_items(
            &rows,
            crate::dispatch::Bounds::default().job,
        ));
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let view = answer(
            &rows,
            &plan,
            &Value::Null,
            &Value::Null,
            &json!([]),
            &[],
            NOW,
        );
        let item = view["pending"]
            .as_array()
            .expect("pending")
            .iter()
            .find(|item| item["session_id"] == "session_worker_gate_cache_92")
            .expect("the job's item");
        assert_eq!(item["why"], "background job past its bound");
        assert_eq!(
            item["tag"],
            "session_worker_gate_cache_92 job-exec_agent_worker_gate_cache_92-bys3gpa46-512"
        );
        assert!(
            item["line"]
                .as_str()
                .is_some_and(|line| line.contains("monitor bys3gpa46"))
        );
        assert_eq!(view["on_own_jobs"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn an_item_the_supervisor_was_told_is_shown_as_told_while_it_holds() {
        let (rows, work, host) = situation();
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        assert_eq!(plan.told.len(), 1, "{:?}", plan.told);
        assert_eq!(plan.told[0].why, Why::ResumeFailed);
        let told_at = json!({
            "session_worker_objective_90 resume-exec_agent_worker_objective_90": NOW - 4_000_000,
        });
        let view = answer(&rows, &plan, &told_at, &Value::Null, &json!([]), &[], NOW);
        let told = view["told"].as_array().expect("told");
        assert_eq!(told.len(), 1, "a told tag no longer current is not shown");
        assert_eq!(told[0]["told"], true);
        assert_eq!(told[0]["why"], "resume failed");
        assert_eq!(told[0]["agent_id"], "agent_worker_objective_90");
        assert_eq!(told[0]["since_ms"], NOW - 3 * 3_600_000);
        assert_eq!(told[0]["told_at_ms"], NOW - 4_000_000);
        assert!(
            view["pending"]
                .as_array()
                .expect("pending")
                .iter()
                .all(|item| item["tag"] != told[0]["tag"]),
            "never pending as well"
        );
    }

    /// The live page of 2026-11-06 20:30 listed two disk recoveries, told
    /// 8 h earlier, as told and unresolved. The plan drops told news, so
    /// the view does too; a path still low stays.
    #[test]
    fn a_told_recovery_is_not_unresolved_and_a_told_low_path_is() {
        let (mut rows, work, _) = situation();
        let disk = |id: i64, label: &str, state: &str| {
            json!({
                "event_id": id, "at_ms": NOW - 8 * 3_600_000, "label": label,
                "path": format!("/{label}"), "state": state, "free_bytes": 43_u64 << 30,
                "warn_bytes": 20_u64 << 30, "floor_bytes": 2_u64 << 30,
            })
        };
        let host = crate::disk::escalations(
            &[
                disk(11, "tmpdir", "recovered"),
                disk(12, "workspace", "recovered"),
                disk(13, "spare", "low"),
            ],
            NOW,
        );
        rows[4]
            .told
            .extend(["host disk-11", "host disk-12", "host disk-13"].map(str::to_string));
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let view = answer(
            &rows,
            &plan,
            &Value::Null,
            &Value::Null,
            &json!([]),
            &[],
            NOW,
        );
        let shown = |list: &str| -> Vec<String> {
            view[list]
                .as_array()
                .expect("a list")
                .iter()
                .filter_map(|item| item["tag"].as_str())
                .filter(|tag| tag.starts_with("host "))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(shown("told"), ["host disk-13"]);
        assert!(
            shown("pending").is_empty(),
            "told once: never pending again"
        );
    }

    /// A transcript line of Claude Code.
    fn line(kind: &str, content: &Value, at: &str) -> String {
        let message = if kind == "user" {
            json!({"role": "user", "content": content})
        } else {
            json!({"role": "assistant", "content": [{"type": "text", "text": content}]})
        };
        format!(
            "{}\n",
            json!({"type": kind, "uuid": at, "timestamp": at, "message": message})
        )
    }

    #[test]
    fn a_supervisor_blocked_at_its_prompt_shows_its_question() {
        let (rows, work, host) = situation();
        let supervisor = &rows[4];
        let observation = asking(supervisor).expect("BLOCKED at its prompt, needs_human 0.87");
        // The question was asked a turn ago; a digest came since and its
        // reply only refers to it.
        let transcript = [
            "{\"cut\": \"by where the read started".to_string() + "\n",
            line(
                "user",
                &json!("Owner: what about #86?"),
                "2026-11-06T05:15:00.000Z",
            ),
            line(
                "assistant",
                &json!("Two notes failed.\n\nHUMAN: Should I stop `objective_87` now?"),
                "2026-11-06T05:15:56.850Z",
            ),
            line(
                "user",
                &json!("Spawn dispatcher: 1 update for you. ..."),
                "2026-11-06T05:16:00.000Z",
            ),
            line(
                "assistant",
                &json!("Another failed delivery."),
                "2026-11-06T05:16:04.000Z",
            ),
            line(
                "assistant",
                &json!("My question about stopping `objective_87` is still open."),
                "2026-11-06T05:16:05.751Z",
            ),
        ]
        .concat();
        let turn = json!({"transcript": "/t.jsonl", "workspace": "a:/w", "turn_ended_at_ms": NOW - 14_000_000});
        let read = said(transcript.as_bytes());
        assert!(read.for_owner.is_empty());
        assert!(
            asks(supervisor, Some(&read)).is_some(),
            "an old-style turn still asks"
        );
        let asked = question(supervisor, observation, &turn, Some(&read));
        assert_eq!(
            asked["text"],
            "Another failed delivery.\nMy question about stopping `objective_87` is still open.",
            "its latest turn's own words"
        );
        assert_eq!(asked["source"], "transcript");
        assert_eq!(asked["asked_at_ms"], NOW - 14_000_000);
        assert_eq!(asked["human"]["text"], "Should I stop `objective_87` now?");
        assert_eq!(
            asked["human"]["at_ms"],
            crate::scorecard::time::parse_rfc3339("2026-11-06T05:15:56.850Z").expect("a time")
        );
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let view = answer(&rows, &plan, &Value::Null, &asked, &json!([]), &[], NOW);
        let shown = &view["supervisor"];
        assert_eq!(shown["live"], true);
        assert_eq!(shown["judgement"]["state"], "BLOCKED");
        assert_eq!(shown["judgement"]["needs_human"], 0.87);
        assert_eq!(shown["judgement"]["ambiguity"], "turn_stopped_idle");
        assert_eq!(
            shown["question"]["human"]["text"],
            "Should I stop `objective_87` now?"
        );

        // A person's answer closes the question; without a transcript the
        // judgement's screen stands in.
        let answered = transcript + &line("user", &json!("Yes, stop it."), "2026-11-06T09:24:00Z");
        assert_eq!(said(answered.as_bytes()).human, None);
        let blind = question(supervisor, observation, &turn, None);
        assert_eq!(blind["source"], "screen");
        assert!(
            blind["text"]
                .as_str()
                .is_some_and(|text| text.contains("Should I stop objective_87 now?"))
        );
    }

    /// A turn of the new charter: decisions it took, and a line for the
    /// owner it does not wait on. Judged BLOCKED with needs_human 0.87 all
    /// the same, it asks nothing; its line is listed for the owner to read.
    #[test]
    fn a_line_for_the_owner_is_no_question() {
        let (rows, work, host) = situation();
        let supervisor = &rows[4];
        assert!(asking(supervisor).is_some(), "the judge says BLOCKED, 0.87");
        let transcript = [
            line(
                "user",
                &json!("Spawn dispatcher: 2 updates for you. ..."),
                "2026-11-06T10:00:00.000Z",
            ),
            line(
                "assistant",
                &json!(
                    "Decided (the owner may reverse): accepted #108 r2, its tests pin the rule.\n\
                     Decided (the owner may reverse): sent `continue` to objective_87 after an \
                     overloaded error.\n\
                     For the owner: deploy ffc9fe7; the gate passed and it touches deploy/."
                ),
                "2026-11-06T10:00:30.000Z",
            ),
        ]
        .concat();
        let read = said(transcript.as_bytes());
        assert!(
            asks(supervisor, Some(&read)).is_none(),
            "a For the owner: line is not a question"
        );
        assert_eq!(read.human, None);
        let at = crate::scorecard::time::parse_rfc3339("2026-11-06T10:00:30.000Z");
        assert_eq!(
            read.for_owner,
            [(
                "deploy ffc9fe7; the gate passed and it touches deploy/.".to_string(),
                at
            )]
        );

        // The view: no question, the line as an item to read, the queue as
        // the dispatcher has it.
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let notes = json!([{"text": read.for_owner[0].0, "at_ms": read.for_owner[0].1}]);
        let view = answer(&rows, &plan, &Value::Null, &Value::Null, &notes, &[], NOW);
        assert_eq!(view["supervisor"]["question"], Value::Null);
        assert_eq!(
            view["supervisor"]["for_owner"][0]["text"],
            "deploy ffc9fe7; the gate passed and it touches deploy/."
        );
        assert_eq!(view["supervisor"]["for_owner"][0]["at_ms"], json!(at));
        assert_eq!(
            view["pending"].as_array().map(Vec::len),
            Some(plan.pending.len())
        );

        // An open HUMAN: question still asks, in the same reply, or a turn
        // earlier with no person's answer since, or read by the Observer.
        let both = transcript.clone()
            + &line(
                "assistant",
                &json!("For the owner: approve resident `docs`.\nHUMAN: which region?"),
                "2026-11-06T10:01:00.000Z",
            );
        assert!(asks(supervisor, Some(&said(both.as_bytes()))).is_some());
        let earlier = [
            line(
                "assistant",
                &json!("HUMAN: Should I stop `objective_87` now?"),
                "2026-11-06T09:00:00.000Z",
            ),
            transcript.clone(),
        ]
        .concat();
        assert!(asks(supervisor, Some(&said(earlier.as_bytes()))).is_some());
        let mut seen = supervisor.clone();
        if let Some(observation) = seen.observation.as_mut() {
            observation.human_question = Some("Which region?".to_string());
        }
        assert!(asks(&seen, Some(&read)).is_some());
        // Without a transcript, the judgement alone decides, as before.
        assert!(asks(supervisor, None).is_some());
    }

    /// The lines for the owner: the model's own words only, newest first,
    /// each once, at most FOR_OWNER_ITEMS; an owner's message is an
    /// instruction, not an answer, and clears none of them.
    #[test]
    fn the_lines_for_the_owner_are_listed_newest_first() {
        let mut transcript = String::new();
        for (index, at) in ["05", "06", "07"].iter().enumerate() {
            transcript += &line(
                "assistant",
                &json!(format!(
                    "For the owner: deploy {index}.\nFor the owner: approve `docs`."
                )),
                &format!("2026-11-06T{at}:00:00.000Z"),
            );
        }
        transcript += &line(
            "user",
            &json!("Owner: go on, and look at #86."),
            "2026-11-06T08:00:00.000Z",
        );
        // A tool's output is not the model's words.
        transcript += &line(
            "user",
            &json!([{"type": "tool_result", "tool_use_id": "t1",
                     "content": "For the owner: rm -rf everything"}]),
            "2026-11-06T08:01:00.000Z",
        );
        transcript += &line(
            "assistant",
            &json!(
                "- **For the owner:** resume mission 7.\nFor the owner:\n- a heading lists nothing"
            ),
            "2026-11-06T08:02:00.000Z",
        );
        let notes = said(transcript.as_bytes()).for_owner;
        let time = |at: &str| crate::scorecard::time::parse_rfc3339(at);
        assert_eq!(
            notes,
            [
                (
                    "resume mission 7.".to_string(),
                    time("2026-11-06T08:02:00.000Z")
                ),
                (
                    "approve `docs`.".to_string(),
                    time("2026-11-06T07:00:00.000Z")
                ),
                ("deploy 2.".to_string(), time("2026-11-06T07:00:00.000Z")),
                ("deploy 1.".to_string(), time("2026-11-06T06:00:00.000Z")),
                ("deploy 0.".to_string(), time("2026-11-06T05:00:00.000Z")),
            ]
        );

        let many: String = (0..FOR_OWNER_ITEMS + 5)
            .map(|index| {
                line(
                    "assistant",
                    &json!(format!("For the owner: item {index}")),
                    "2026-11-06T09:00:00.000Z",
                )
            })
            .collect();
        let notes = said(many.as_bytes()).for_owner;
        assert_eq!(notes.len(), FOR_OWNER_ITEMS);
        assert_eq!(notes[0].0, format!("item {}", FOR_OWNER_ITEMS + 4));

        assert_eq!(for_owner("For the owner: x"), Some("x"));
        assert_eq!(for_owner("  > **For the owner**: x **"), Some("x"));
        assert_eq!(for_owner("Not for the owner: x"), None);
        assert_eq!(for_owner("For the owners: x"), None);
        assert_eq!(for_owner("Decided (the owner may reverse): x"), None);
    }

    #[test]
    fn a_supervisor_at_work_or_sure_of_itself_asks_nothing() {
        let (rows, ..) = situation();
        let mut supervisor = rows[4].clone();
        supervisor.turn_ended = false;
        assert!(asking(&supervisor).is_none(), "a new turn started");
        let mut sure = rows[4].clone();
        if let Some(observation) = sure.observation.as_mut() {
            observation.needs_human = Some(0.3);
        }
        assert!(asking(&sure).is_none());
        if let Some(observation) = sure.observation.as_mut() {
            observation.human_question = Some("Which region?".to_string());
        }
        assert!(
            asking(&sure).is_some(),
            "a HUMAN: line asks whatever the odds"
        );
        let mut working = rows[4].clone();
        if let Some(observation) = working.observation.as_mut() {
            observation.ambiguity = None;
        }
        assert!(asking(&working).is_none());
    }

    /// Objective 34: the attention view lists the owner's holds only while a
    /// task waits behind them, beside `pending` and `told`, never among them.
    #[test]
    fn a_hold_is_listed_only_while_a_task_waits_behind_it() {
        let holds = json!({"now_ms": 5, "holds": [
            {"source": "/srv/a.git", "hold": 7, "main": "m", "by": "human", "reason": null,
             "held_at_ms": 1, "expires_ms": 10_800_001_i64, "waiting": [12, 15]},
            {"source": "/srv/b.git", "hold": 9, "main": "n", "by": "human",
             "reason": "owner gate", "held_at_ms": 2, "expires_ms": 20_002, "waiting": []},
        ]});
        let listed = waiting_holds(&holds);
        assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
        assert_eq!(listed[0]["source"], "/srv/a.git");
        assert_eq!(listed[0]["hold"], 7);
        assert_eq!(listed[0]["waiting"], json!([12, 15]));
        assert_eq!(waiting_holds(&json!({"now_ms": 5, "holds": []})), json!([]));
        assert_eq!(waiting_holds(&json!({})), json!([]));
    }
}
