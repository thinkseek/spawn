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
//!     "for_owner": [{"text": "deploy ffc9fe7", "at_ms": 1793957000000 | null,
//!                    "tasks": [{"id": 198, "state": "accepted"}],  // the tasks it names
//!                    "settled": false}]   // a review ask whose tasks have all ended
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
//! and its judgement of that turn (one that rests on the turn's end, never
//! an earlier turn's) asks for a person: BLOCKED or WAITING with
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
//!
//! A `For the owner:` line with no words after it (`**For the owner:**`)
//! is a heading: each list item right under it (`- `, `* ` or `<n>. `,
//! after any indent), up to a blank line or a line that is no item, is a
//! line of its own, its words without the marker, at its turn's time
//! (objective 070). The heading is none, and a list under any other heading
//! is not the owner's.
//!
//! Each line says what became of the tasks it names (objective 064: a line
//! saying four reports waited for review stood for 30 h when all four had
//! ended before it was written). `tasks` are the tasks the line names, in
//! its order, each once, `{id, state}` with the task's state now (read for
//! those tasks alone); a number that names no task is left out. A line
//! names a task as `T<n>`, or as a number (bare or `#<n>`) right after the
//! word task, tasks, report or reports, a list included ("reports 198, 204,
//! 208 and 211": a comma, "and", or both, before each next number). A bare
//! `#<n>` elsewhere names none ("mission #4", "issue #54"), nor does a
//! range's end ("tasks 160 to 167" names 160). `settled` is true when the
//! line's ask is to review, accept or merge the tasks (a word beginning
//! "review", "accept" or "merg", and none beginning "deploy"), it names a
//! task at least, and every task it names has ended (accepted, landed or
//! cancelled); false otherwise. A line with any other ask (a deploy, an
//! init-db, a decision) is never settled, whatever became of its tasks:
//! hiding a live ask is the worse failure. Home's card leaves a settled
//! line out (web/src/home.ts `latestNotes`); the supervisor's full list
//! still shows it, marked settled.

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
    let (waits, _) = crate::deliver::Waits::from_env();
    let reading = dispatch::read_plan(
        &store,
        dispatch::load(&store, waits)?,
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
    let for_owner = owner_notes(
        &store,
        said.as_ref()
            .map_or(&[][..], |said| said.for_owner.as_slice()),
        &mut warnings,
    );
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
        &for_owner,
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

/// What the supervisor has to dispatch, read as [`attention`] reads it, for
/// the alert step's `agent_stuck` (crate::alerts, objective 057): `{question,
/// items}`, `question` whether it asks the owner something now (the
/// attention view's `supervisor.question`), `items` the plan's `pending` and
/// `told` escalations, each `{tag, session_id, agent_id, why, judgement}`,
/// `judgement` true for one that is a worker's own judgement (attention,
/// stopped at its prompt, a nudge that did not help): the alert step decides
/// which of them are the supervisor's to act on. `lines` are the plan's log
/// lines, for the loop's log.
pub(crate) fn supervisor_dispatch(store: &spawn_postgres::PostgresStore) -> Result<Value, String> {
    let disk = store.host_disk().map_err(super::err)?;
    let (bounds, _) = dispatch::Bounds::from_env();
    let (waits, _) = crate::deliver::Waits::from_env();
    let reading = dispatch::read_plan(
        store,
        dispatch::load(store, waits)?,
        |now| crate::disk::escalations(disk.as_array().map_or(&[][..], Vec::as_slice), now),
        &bounds,
        crate::config::supervised(),
        &crate::config::spawn_program(),
        None,
    )?;
    let supervisor = reading
        .rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION);
    // The transcript is read only when the judgement may ask: most passes
    // have no question, and read nothing more.
    let question = supervisor.is_some_and(|facts| {
        asking(facts).is_some() && {
            let said = store
                .latest_turn(SUPERVISOR_SESSION)
                .ok()
                .and_then(|turn| transcript_tail(&turn))
                .map(|bytes| said(&bytes));
            asks(facts, said.as_ref()).is_some()
        }
    });
    let agents: BTreeMap<&str, &str> = reading
        .rows
        .iter()
        .map(|facts| (facts.session_id.as_str(), facts.agent_id.as_str()))
        .collect();
    let items: Vec<Value> = reading
        .plan
        .pending
        .iter()
        .chain(&reading.plan.told)
        .map(|escalation| {
            json!({
                "tag": escalation.tag(),
                "session_id": escalation.session_id,
                "agent_id": agents.get(escalation.session_id.as_str()),
                "why": dispatch::heading(escalation.why),
                "judgement": matches!(
                    escalation.why,
                    dispatch::Why::Attention | dispatch::Why::Idle | dispatch::Why::NudgeFailed
                ),
            })
        })
        .collect();
    // The plan's once-said log lines (an order no task can meet) are said
    // by whoever reads the plan first in this process: the loop logs them.
    Ok(json!({ "question": question, "items": items, "lines": reading.lines }))
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
/// `HUMAN:` line in its reply. The latest judgement, current or not (a
/// screen reading after it changes nothing the turn's end does not), but
/// one of this turn: it rests on the turn's end ([`Facts::turn_seq`]) or on
/// a later fact. A judgement of an earlier turn says nothing of the one
/// that ended now: once a person answers and the answer's turn ends, the
/// question that was answered would otherwise be back until the Observer
/// judges again (GitHub #16 item 9).
fn asking(facts: &Facts) -> Option<&Observation> {
    let observation = facts.observation.as_ref()?;
    let of_this_turn = facts
        .turn_seq
        .is_some_and(|turn| observation.based_on_seq.0 >= turn);
    let at_prompt = facts.live
        && facts.turn_ended
        && of_this_turn
        && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle);
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

/// Task states in which a task has ended: a review ask about it is settled.
const ENDED: [&str; 3] = ["accepted", "landed", "cancelled"];

/// Words a number right after names a task (any case): `task 5`, `reports
/// 2, 3 and 4`.
const TASK_WORDS: [&str; 4] = ["task", "tasks", "report", "reports"];

/// The supervisor's lines for the owner as `/api/attention` lists them,
/// `[{text, at_ms, tasks, settled}]` ([`owner_note`]), the named tasks'
/// states read for those tasks alone. When they cannot be read, every line
/// names no task and none is settled, and a warning says why.
pub(super) fn owner_notes(
    store: &spawn_postgres::PostgresStore,
    notes: &[(String, Option<i64>)],
    warnings: &mut Vec<String>,
) -> Value {
    let mut ids: Vec<i64> = notes
        .iter()
        .flat_map(|(text, _)| named_tasks(text))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let states = if ids.is_empty() {
        BTreeMap::new()
    } else {
        match store.task_label_facts(&ids) {
            Ok(facts) => task_states(&facts),
            Err(error) => {
                warnings.push(format!(
                    "attention: the tasks the owner's lines name: {error}"
                ));
                BTreeMap::new()
            }
        }
    };
    Value::Array(
        notes
            .iter()
            .map(|(text, at_ms)| owner_note(text, *at_ms, &states))
            .collect(),
    )
}

/// `{id: state}` of a board's tasks (`{missions: [{tasks: [{id, state}]}]}`).
fn task_states(board: &Value) -> BTreeMap<i64, String> {
    board["missions"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|mission| mission["tasks"].as_array().into_iter().flatten())
        .filter_map(|task| Some((task["id"].as_i64()?, task["state"].as_str()?.to_string())))
        .collect()
}

/// One line for the owner, `{text, at_ms, tasks, settled}`: `tasks` the
/// tasks it names ([`named_tasks`]) that `states` holds, `{id, state}` in
/// the line's order; `settled` when its ask is a review of them
/// ([`review_ask`]), it names one at least, and every one has ended
/// ([`ENDED`]).
fn owner_note(text: &str, at_ms: Option<i64>, states: &BTreeMap<i64, String>) -> Value {
    let tasks: Vec<(i64, &str)> = named_tasks(text)
        .into_iter()
        .filter_map(|id| states.get(&id).map(|state| (id, state.as_str())))
        .collect();
    let settled = review_ask(text)
        && !tasks.is_empty()
        && tasks.iter().all(|(_, state)| ENDED.contains(state));
    json!({
        "text": text,
        "at_ms": at_ms,
        "tasks": tasks
            .iter()
            .map(|(id, state)| json!({"id": id, "state": state}))
            .collect::<Vec<_>>(),
        "settled": settled,
    })
}

/// A piece of a line, as [`named_tasks`] and [`review_ask`] read it.
#[derive(Debug, PartialEq)]
enum Token {
    /// Letters, then any letters or digits (`reports`, `T12`).
    Word(String),
    /// Digits, after a `#` or not (`5`, `#5`); one too long for an id is
    /// [`Token::Other`].
    Number(i64),
    Comma,
    /// Any other mark (`;`, `(`, `-`, `/`, a lone `#`).
    Other,
}

fn tokens(line: &str) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let digit_at = |index: usize| chars.get(index).is_some_and(char::is_ascii_digit);
        if c.is_whitespace() {
            at += 1;
        } else if c.is_alphabetic() {
            let start = at;
            while at < chars.len() && chars[at].is_alphanumeric() {
                at += 1;
            }
            out.push(Token::Word(chars[start..at].iter().collect()));
        } else if c.is_ascii_digit() || (c == '#' && digit_at(at + 1)) {
            if c == '#' {
                at += 1;
            }
            let start = at;
            while digit_at(at) {
                at += 1;
            }
            let digits: String = chars[start..at].iter().collect();
            out.push(digits.parse().map_or(Token::Other, Token::Number));
        } else {
            out.push(if c == ',' { Token::Comma } else { Token::Other });
            at += 1;
        }
    }
    out
}

/// `T<n>`: a task by its short name.
fn short_name(word: &str) -> Option<i64> {
    let digits = word.strip_prefix('T')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The task ids a line names, in its order, each once: `T<n>`, and a
/// number (bare or `#<n>`) right after the word task, tasks, report or
/// reports, with the list it starts (`reports 198, 204, 208 and 211`: a
/// comma, "and", or both, before each next number). A `#<n>` or number
/// anywhere else names none (`mission #4`), nor does a range's end (`tasks
/// 160 to 167` names 160).
fn named_tasks(line: &str) -> Vec<i64> {
    let tokens = tokens(line);
    let mut ids: Vec<i64> = Vec::new();
    let mut push = |id: i64| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    let is_and = |token: Option<&Token>| matches!(token, Some(Token::Word(word)) if word.eq_ignore_ascii_case("and"));
    let mut at = 0;
    while at < tokens.len() {
        let Token::Word(word) = &tokens[at] else {
            at += 1;
            continue;
        };
        at += 1;
        if let Some(id) = short_name(word) {
            push(id);
            continue;
        }
        if !TASK_WORDS.contains(&word.to_lowercase().as_str()) {
            continue;
        }
        let Some(Token::Number(first)) = tokens.get(at) else {
            continue;
        };
        push(*first);
        at += 1;
        loop {
            let mut next = at;
            if tokens.get(next) == Some(&Token::Comma) {
                next += 1;
            }
            if is_and(tokens.get(next)) {
                next += 1;
            }
            match tokens.get(next) {
                Some(Token::Number(id)) if next > at => {
                    push(*id);
                    at = next + 1;
                }
                _ => break,
            }
        }
    }
    ids
}

/// Whether a line asks the owner to review, accept or merge (a word that
/// begins "review", "accept" or "merg") and asks no deploy (none begins
/// "deploy"): the only ask the tasks' ending settles.
fn review_ask(line: &str) -> bool {
    let words: Vec<String> = tokens(line)
        .into_iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word.to_lowercase()),
            _ => None,
        })
        .collect();
    let begins = |prefix: &str| words.iter().any(|word| word.starts_with(prefix));
    (begins("review") || begins("accept") || begins("merg")) && !begins("deploy")
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
                let words = owner_lines(&text);
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

/// The lines a reply's `text` has for the owner, in its order: each inline
/// [`FOR_OWNER`] line's words, and each list item right under a heading (a
/// [`FOR_OWNER`] line with no words after it: `**For the owner:**`), up to
/// a blank line or a line that is no item ([`list_item`]). The heading
/// itself is none, and a list under any other heading is not the owner's
/// (objective 070: 19 of the supervisor's 217 owner blocks were headings
/// over lists, and none of their items reached Home).
fn owner_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut under_heading = false;
    for line in text.lines() {
        if under_heading {
            if let Some(item) = list_item(line) {
                // An item that is itself an inline line keeps only its words.
                let words = for_owner(item).filter(|words| !words.is_empty()).unwrap_or(item);
                if !words.is_empty() {
                    lines.push(words);
                }
                continue;
            }
            under_heading = false;
        }
        match for_owner(line) {
            Some("") => under_heading = true,
            Some(words) => lines.push(words),
            None => {}
        }
    }
    lines
}

/// A list item's words without its marker (`- `, `* ` or `<n>. `, after
/// any indent); `None` for a line that is no item, a blank one included.
fn list_item(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let words = if let Some(words) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        words
    } else {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        rest[digits..].strip_prefix(". ")?
    };
    Some(words.trim())
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
    use super::{
        FOR_OWNER_ITEMS, answer, asking, asks, for_owner, named_tasks, owner_note, question,
        review_ask, said, task_states, waiting_holds,
    };
    use crate::deliver::Work;
    use crate::dispatch::{Escalation, Facts, Why, decide_with};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

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
            "turn_seq": observation["based_on_seq"],
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

    /// GitHub #16 item 9: the supervisor asked, a person answered, and the
    /// answer's turn has ended. Until the Observer judges that turn, the
    /// latest judgement is still the one of the turn that asked: it is no
    /// question of the turn that ended now, and must not return to the
    /// queue (it did: BLOCKED 0.87 with the turn ended passed again).
    #[test]
    fn an_answered_question_does_not_return_when_the_answers_turn_ends() {
        let judged_at = |seq: u64| judged("BLOCKED", 0.87, false, seq);
        let supervisor = |observation: &Value, evidence: u64, turn: u64, ended: bool| {
            let mut row = row(
                "session_supervisor",
                "agent_supervisor",
                observation,
                NOW - 60_000,
            );
            row["evidence_seq"] = json!(evidence);
            row["turn_seq"] = json!(turn);
            row["turn_ended"] = json!(ended);
            facts(&row)
        };
        // Its turn ended at 1351 and was judged there: it asks.
        let asked = supervisor(&judged_at(1351), 1351, 1351, true);
        assert!(asking(&asked).is_some());
        // A later fact that is no turn (a screen reading) changes nothing.
        assert!(asking(&supervisor(&judged_at(1351), 1355, 1351, true)).is_some());
        // The answer is typed (1356, a new turn: `message.received`).
        assert!(asking(&supervisor(&judged_at(1351), 1356, 1356, false)).is_none());
        // The answer's turn ends at 1362; the judgement is still 1351's.
        let answered = supervisor(&judged_at(1351), 1362, 1362, true);
        assert!(
            asking(&answered).is_none(),
            "the judgement of an earlier turn is no question of this one"
        );
        assert!(asks(&answered, None).is_none());
        // Judged again on the turn that ended: what it says then stands.
        let again = supervisor(&judged_at(1362), 1362, 1362, true);
        assert!(asking(&again).is_some(), "it asks again, in this turn");
        let mut done = judged_at(1363);
        done["state"] = json!("COMPLETED");
        done["needs_human"] = json!(0.1);
        assert!(asking(&supervisor(&done, 1363, 1362, true)).is_none());
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

    #[test]
    fn a_line_names_its_tasks_in_each_form() {
        assert_eq!(
            named_tasks("task 2 landed; deploying it needs init-db"),
            [2]
        );
        assert_eq!(named_tasks("task #5 waits for your review."), [5]);
        assert_eq!(named_tasks("Task 7 and T12 are done"), [7, 12]);
        assert_eq!(named_tasks("Report 9 is in"), [9]);
        assert_eq!(named_tasks("T3, T4 ready"), [3, 4]);
        assert_eq!(
            named_tasks("tasks 160 to 167 land next"),
            [160],
            "a range names its first"
        );
        assert_eq!(named_tasks("tasks 160-167"), [160]);
        assert_eq!(
            named_tasks("mission #3 has no backlog yet."),
            Vec::<i64>::new()
        );
        assert_eq!(named_tasks("see issue #54 and PR 12"), Vec::<i64>::new());
        assert_eq!(named_tasks("deploy main (3 commits)"), Vec::<i64>::new());
        assert_eq!(named_tasks("Tea12 and t12 name none"), Vec::<i64>::new());
        assert_eq!(named_tasks("task 5 again, task #5"), [5], "each once");
        assert_eq!(named_tasks("tasks"), Vec::<i64>::new());
    }

    #[test]
    fn a_list_after_reports_names_every_number_in_it() {
        assert_eq!(named_tasks("reports 2, 3 and 4 wait"), [2, 3, 4]);
        assert_eq!(named_tasks("tasks #2, #3, and #4"), [2, 3, 4]);
        assert_eq!(named_tasks("tasks 8 and 9"), [8, 9]);
        assert_eq!(
            named_tasks("tasks 2, mission 3"),
            [2],
            "a comma before a word ends the list"
        );
        // The live line (2026-11-11 06:23Z): four reports, two missions.
        assert_eq!(
            named_tasks(
                "reports 198, 204, 208 and 211 (missions 7/8) are still waiting for your ccd \
                 review"
            ),
            [198, 204, 208, 211]
        );
    }

    #[test]
    fn only_a_review_accept_or_merge_ask_without_a_deploy_can_settle() {
        assert!(review_ask("task #5 waits for your review."));
        assert!(review_ask("Accept T4 when you can"));
        assert!(review_ask("merging reports 2 and 3 is yours"));
        assert!(!review_ask(
            "task 2 landed; deploying it needs spawn init-db."
        ));
        assert!(!review_ask("review T4, then deploy main"));
        assert!(!review_ask("decide whether task 3 should go on"));
    }

    #[test]
    fn a_line_is_settled_only_when_its_review_ask_names_tasks_that_all_ended() {
        let states = task_states(&json!({"missions": [
            {"id": 1, "tasks": [{"id": 2, "state": "accepted"}, {"id": 3, "state": "landed"},
                                {"id": 4, "state": "cancelled"}, {"id": 5, "state": "reported"}]},
            {"id": 2, "tasks": [{"id": 198, "state": "accepted"}, {"id": 204, "state": "accepted"},
                                {"id": 208, "state": "cancelled"}, {"id": 211, "state": "accepted"}]}
        ]}));
        let note = |text: &str| owner_note(text, Some(7), &states);
        assert_eq!(
            note("reports 2, 3 and 4 are still waiting for your review."),
            json!({"text": "reports 2, 3 and 4 are still waiting for your review.", "at_ms": 7,
                   "tasks": [{"id": 2, "state": "accepted"}, {"id": 3, "state": "landed"},
                             {"id": 4, "state": "cancelled"}],
                   "settled": true})
        );
        let live = note(
            "reports 198, 204, 208 and 211 (missions 7/8) are still waiting for your ccd review",
        );
        assert_eq!(live["settled"], json!(true), "{live}");
        assert_eq!(live["tasks"].as_array().map(Vec::len), Some(4));
        // A number naming no task is left out; one task still open keeps it.
        let open = note("reports 2, 99 and 5 wait for your review");
        assert_eq!(
            open["tasks"],
            json!([{"id": 2, "state": "accepted"}, {"id": 5, "state": "reported"}])
        );
        assert_eq!(open["settled"], json!(false));
        // Another ask stands, whatever became of its tasks.
        let init = note("task 2 landed; deploying it needs spawn init-db.");
        assert_eq!(init["tasks"], json!([{"id": 2, "state": "accepted"}]));
        assert_eq!(init["settled"], json!(false));
        // No task named, or none known: never settled.
        assert_eq!(note("mission #3 has no backlog yet.")["tasks"], json!([]));
        assert_eq!(
            note("mission #3 has no backlog yet.")["settled"],
            json!(false)
        );
        let unknown = note("task 99 waits for your review");
        assert_eq!(unknown["tasks"], json!([]));
        assert_eq!(unknown["settled"], json!(false));
        let unread = owner_note("review task 2", None, &BTreeMap::new());
        assert_eq!(unread["at_ms"], Value::Null);
        assert_eq!(unread["settled"], json!(false));
    }
}
