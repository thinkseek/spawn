//! `GET /api/attention`: the UI's "Needs a human" queue, computed by the
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
//!     "why": "stalled",            // the digest's heading
//!     "line": "...", "tag": "session_worker_a stall-7-activity-41",
//!     "since_ms": 1793957000000 | null,  // when the record it rests on was written
//!     "told": false, "told_at_ms": null
//!   }],
//!   "told": [ ...the same, "told": true, "told_at_ms": <first digest that carried it> | null ],
//!   "supervisor": null | {
//!     "session_id", "agent_id", "live", "execution_id", "end_cause",
//!     "busy",                      // a command into it is in flight
//!     "turn_ended",                // its live execution's latest turn has ended
//!     "judgement": null | {state, needs_human, stuck, ambiguity, confidence, judge,
//!                          attention, attention_reason, judged_at_ms, current},
//!     "question": null | {
//!       "text": "...",             // its last reply (the latest turn's own words)
//!       "source": "transcript" | "screen" | null,
//!       "asked_at_ms": 1793957000000 | null,  // the end of that turn
//!       "human": null | {"text": "Should I ...?", "at_ms": ... | null}
//!     }
//!   },
//!   "warnings": ["dispatch: stalls: ..."]   // what could not be read
//! }
//! ```
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
    let question = match supervisor.and_then(|facts| asking(facts).map(|seen| (facts, seen))) {
        Some((facts, observation)) => match store.latest_turn(SUPERVISOR_SESSION) {
            Ok(turn) => question(facts, observation, &turn, transcript_tail(&turn).as_deref()),
            Err(error) => {
                warnings.push(format!("attention: the supervisor's turn: {error}"));
                question(facts, observation, &Value::Null, None)
            }
        },
        None => Value::Null,
    };
    Ok(answer(
        &reading.rows,
        &reading.plan,
        &told_at,
        question,
        &warnings,
        super::now_ms(),
    ))
}

/// The answer from what was read: `plan` as [`dispatch::decide_with`] made
/// it from `rows`, when each told item was first told (`told_at`, `{tag:
/// ms}`), the supervisor's `question` ([`question`], or null) and the
/// `warnings` of the reads.
fn answer(
    rows: &[Facts],
    plan: &Plan,
    told_at: &Value,
    question: Value,
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
        .map_or(Value::Null, |facts| supervisor(facts, question));
    json!({
        "generated_at_ms": generated_at_ms,
        "pending": plan.pending.iter().map(|escalation| item(escalation, false)).collect::<Vec<_>>(),
        "told": plan.told.iter().map(|escalation| item(escalation, true)).collect::<Vec<_>>(),
        "supervisor": supervisor,
        "warnings": warnings,
    })
}

/// The supervisor's liveness, its latest judgement and its `question`.
fn supervisor(facts: &Facts, question: Value) -> Value {
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
    let at_prompt = facts.live
        && facts.turn_ended
        && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle);
    let unsure = matches!(
        observation.state,
        SemanticState::Blocked | SemanticState::Waiting
    ) && observation
        .needs_human
        .is_some_and(|value| value >= ASKS_NEEDS_HUMAN);
    (at_prompt && (unsure || observation.human_question.is_some())).then_some(observation)
}

/// What the supervisor asks, from the end of its transcript (`transcript`,
/// none when it could not be read) or else the screen tail stored with its
/// judgement; asked when its latest turn ended (`turn`, from
/// `PostgresStore::latest_turn`), else when it was judged.
fn question(
    facts: &Facts,
    observation: &Observation,
    turn: &Value,
    transcript: Option<&[u8]>,
) -> Value {
    let asked_at = turn["turn_ended_at_ms"]
        .as_i64()
        .or(facts.observation_at_ms);
    let said = transcript.map(said).filter(|said| said.reply.is_some());
    let (text, source, human) = match said {
        Some(said) => (said.reply, Some("transcript"), said.human),
        None => {
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
        }
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
            }
            _ => {}
        }
    }
    Said {
        reply: reply.text().map(str::to_string),
        human,
    }
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
    crate::scorecard::parse_rfc3339(value["timestamp"].as_str()?)
}

#[cfg(test)]
mod tests {
    use super::{answer, asking, question, said};
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
        let view = answer(&rows, &plan, &Value::Null, Value::Null, &[], NOW);
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

    #[test]
    fn an_item_the_supervisor_was_told_is_shown_as_told_while_it_holds() {
        let (rows, work, host) = situation();
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        assert_eq!(plan.told.len(), 1, "{:?}", plan.told);
        assert_eq!(plan.told[0].why, Why::ResumeFailed);
        let told_at = json!({
            "session_worker_objective_90 resume-exec_agent_worker_objective_90": NOW - 4_000_000,
        });
        let view = answer(&rows, &plan, &told_at, Value::Null, &[], NOW);
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
            line("user", &json!("Owner: what about #86?"), "2026-11-06T05:15:00.000Z"),
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
        let asked = question(supervisor, observation, &turn, Some(transcript.as_bytes()));
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
            crate::scorecard::parse_rfc3339("2026-11-06T05:15:56.850Z").expect("a time")
        );
        let plan = decide_with(&rows, &work, &host, true, NOW, "spawn");
        let view = answer(&rows, &plan, &Value::Null, asked, &[], NOW);
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
        assert!(asking(&sure).is_some(), "a HUMAN: line asks whatever the odds");
        let mut working = rows[4].clone();
        if let Some(observation) = working.observation.as_mut() {
            observation.ambiguity = None;
        }
        assert!(asking(&working).is_none());
    }
}
