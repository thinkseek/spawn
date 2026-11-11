//! An agent answers a message by its id (objective 046;
//! docs/fleet-architecture.md §3.4, "Messages").
//!
//! `spawn ack <command id> done|declined [<why>]`, run by an agent in its
//! own pane, answers the message with that id: done, or declined and why.
//! The pane has no database login, so the ack is a file the loop reads, as
//! it reads a report: [`command`] writes it under the workspace the pane
//! was launched in (`SPAWN_WORKSPACE`, which the Bridge sets; never the
//! current directory), in `.spawn/acks/`, which no checkpoint takes, and
//! [`collect`] reads each agent's acks on every dispatcher pass and records
//! each once (`message.acted`, spawn-postgres's `acks`). An ack made while
//! the loop is down waits in the workspace until it is back.
//!
//! Whose ack it is, is the workspace's: the loop records it for the agent
//! whose workspace held it, and for no message but one into that agent's
//! session. An ack carries no session or agent id, so setting
//! `SPAWN_SESSION_ID` or writing a file by hand answers nothing of another
//! agent's. An ack of an unknown id, of a command that types nothing, or of
//! a task's delivery (the task's report answers it: [`report_answers`])
//! records nothing, and the loop's log says so.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spawn_host::agent_files;
use spawn_host::tree_path::TreePath;
use spawn_postgres::{AckRecord, PostgresStore};

use crate::dispatch::Facts;

/// Where an agent's acks wait for the loop, relative to its workspace.
pub(crate) const ACKS: &str = ".spawn/acks";

/// The most of one ack file the loop reads.
const ACK_LIMIT: u64 = 64 * 1024;

/// The longest why recorded, in characters.
const WHY_CHARS: usize = 2000;

const USAGE: &str = "usage: spawn ack <command id> done|declined [<why>]";

/// One ack, as `spawn ack` writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ack {
    pub(crate) command_id: i64,
    pub(crate) outcome: String,
    pub(crate) why: Option<String>,
}

/// The ack the words after `spawn ack` say: the id, the outcome, and the
/// why (the words after the outcome, joined by spaces), required for a
/// decline.
pub(crate) fn parse(words: &[String]) -> Result<Ack, String> {
    let (text, rest) = words
        .split_first()
        .ok_or_else(|| format!("no message id; {USAGE}"))?;
    // Digits as an id prints them: `007` or `+7` is no id.
    let command_id = text
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0 && id.to_string() == *text)
        .ok_or_else(|| format!("{text}: not a message id (a command id, a number); {USAGE}"))?;
    let (outcome, why) = rest
        .split_first()
        .ok_or_else(|| format!("no outcome (done or declined); {USAGE}"))?;
    if outcome != "done" && outcome != "declined" {
        return Err(format!(
            "{outcome}: the outcome is done or declined; {USAGE}"
        ));
    }
    let why = why.join(" ");
    let why = why.trim();
    let why = (!why.is_empty()).then(|| why.chars().take(WHY_CHARS).collect::<String>());
    if outcome == "declined" && why.is_none() {
        return Err(format!(
            "a decline says why: spawn ack {command_id} declined <why>"
        ));
    }
    Ok(Ack {
        command_id,
        outcome: outcome.clone(),
        why,
    })
}

/// The workspace of the pane this runs in, from what the pane was given at
/// launch: `SPAWN_WORKSPACE` (the Bridge's), an absolute directory named
/// after the pane's agent (`SPAWN_AGENT_ID`): where the loop reads the
/// agent's acks. Outside an agent's pane, none.
fn pane_workspace(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let outside =
        |what: &str| format!("spawn ack runs in an agent's own pane: {what}; nothing was recorded");
    let agent = env("SPAWN_AGENT_ID")
        .filter(|agent| !agent.is_empty())
        .ok_or_else(|| outside("SPAWN_AGENT_ID is not set"))?;
    env("SPAWN_EXECUTION_ID")
        .filter(|execution| !execution.is_empty())
        .ok_or_else(|| outside("SPAWN_EXECUTION_ID is not set"))?;
    let workspace = env("SPAWN_WORKSPACE")
        .filter(|workspace| !workspace.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            outside("SPAWN_WORKSPACE is not set (an engine started before `spawn ack` existed)")
        })?;
    if !workspace.is_absolute() || !workspace.is_dir() {
        return Err(outside(&format!(
            "SPAWN_WORKSPACE {} is not a directory",
            workspace.display()
        )));
    }
    if workspace.file_name().and_then(|name| name.to_str()) != Some(agent.as_str()) {
        return Err(outside(&format!(
            "SPAWN_WORKSPACE {} is not {agent}'s workspace",
            workspace.display()
        )));
    }
    Ok(workspace)
}

/// Write `ack` into `workspace`'s acks, whole: a hidden file first, then
/// renamed into place, so the loop never reads half of one. Named by this
/// host's clock and the process, so acks sort in the order they were made.
fn write(workspace: &Path, ack: &Ack) -> Result<PathBuf, String> {
    let directory = workspace.join(ACKS);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let name = format!("{nanos:024}-{}.json", std::process::id());
    let hidden = directory.join(format!(".{name}"));
    let path = directory.join(&name);
    let bytes = serde_json::to_vec(ack).map_err(|error| error.to_string())?;
    std::fs::write(&hidden, bytes)
        .map_err(|error| format!("write {}: {error}", hidden.display()))?;
    std::fs::rename(&hidden, &path).map_err(|error| {
        let _ = std::fs::remove_file(&hidden);
        format!("move {} into place: {error}", hidden.display())
    })?;
    Ok(path)
}

/// `spawn ack <command id> done|declined [<why>]`: 0 when the ack is
/// written where the loop reads it; an error, and nothing written, for a
/// malformed ack or outside an agent's pane.
pub(crate) fn command(words: &[String]) -> Result<i32, String> {
    let ack = parse(words)?;
    let workspace = pane_workspace(&|name| std::env::var(name).ok())?;
    write(&workspace, &ack)?;
    println!(
        "spawn: ack of message #{} ({}) taken; the loop records it",
        ack.command_id, ack.outcome
    );
    Ok(0)
}

/// Read and record the acks waiting in each agent's workspace under `root`
/// (the agents of `rows`), oldest first: each is recorded or refused once,
/// then removed; one the store could not take stays for the next pass.
/// Returns log lines.
pub(crate) fn collect(store: &PostgresStore, rows: &[Facts], root: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    let mut agents: Vec<&str> = rows.iter().map(|facts| facts.agent_id.as_str()).collect();
    agents.sort_unstable();
    agents.dedup();
    for agent in agents {
        let workspace = root.join(agent);
        let acks = TreePath::from(ACKS);
        let mut names = match agent_files::list(&workspace, &acks) {
            Ok(names) => names,
            Err(error) => {
                lines.push(format!("ack: {agent}: {error}"));
                continue;
            }
        };
        names.retain(|name| !name.starts_with(b".") && name.ends_with(b".json"));
        names.sort();
        for name in names {
            let Ok(name) = String::from_utf8(name) else {
                continue;
            };
            let path = TreePath::from(format!("{ACKS}/{name}").as_str());
            match one(store, agent, &workspace, &path) {
                Ok(line) => {
                    lines.push(line);
                    let _ = agent_files::remove(&workspace, &path);
                }
                Err(error) => lines.push(format!("ack: {agent}: {name}: {error}")),
            }
        }
    }
    lines
}

/// Record one ack of `agent`'s, at `path` in its `workspace`: the log line,
/// or the store's error (the file stays).
fn one(
    store: &PostgresStore,
    agent: &str,
    workspace: &Path,
    path: &TreePath,
) -> Result<String, String> {
    let bytes = match agent_files::read_regular(workspace, path, ACK_LIMIT) {
        Ok(Some((bytes, _, false))) => bytes,
        Ok(Some((_, _, true))) => return Ok(format!("ack: {agent}: {path} is too long; ignored")),
        Ok(None) => return Ok(format!("ack: {agent}: {path} is gone")),
        Err(error) => return Ok(format!("ack: {agent}: {path}: {error}; ignored")),
    };
    let ack = match serde_json::from_slice::<Ack>(&bytes) {
        Ok(ack) if ack.outcome == "done" || ack.outcome == "declined" => ack,
        Ok(ack) => {
            return Ok(format!(
                "ack: {agent}: {path}: outcome {} is not done or declined; ignored",
                ack.outcome
            ));
        }
        Err(error) => return Ok(format!("ack: {agent}: {path} is no ack ({error}); ignored")),
    };
    if ack.outcome == "declined" && ack.why.as_deref().is_none_or(|why| why.trim().is_empty()) {
        return Ok(format!(
            "ack: {agent}: message #{} declined with no why; ignored",
            ack.command_id
        ));
    }
    let why = ack
        .why
        .as_deref()
        .map(|why| why.chars().take(WHY_CHARS).collect::<String>());
    match store
        .record_ack(agent, ack.command_id, &ack.outcome, why.as_deref())
        .map_err(|error| error.to_string())?
    {
        AckRecord::Recorded { session_id } => Ok(format!(
            "ack: message #{} {} by {agent} ({session_id})",
            ack.command_id, ack.outcome
        )),
        AckRecord::Answered => Ok(format!(
            "ack: message #{} was answered before; {agent}'s {} recorded nothing",
            ack.command_id, ack.outcome
        )),
        AckRecord::Refused(why) => Ok(format!(
            "ack: {agent}'s ack of message #{} recorded nothing: {why}",
            ack.command_id
        )),
    }
}

/// How far back each pass looks for reports whose delivery is unanswered:
/// a day. The first pass of a loop looks at every report, so one missed
/// while it was down is answered once it is back.
const REPORTS_BACK_MS: i64 = 86_400_000;

/// What a report's Outcome answers its delivery with: `done` for an
/// Outcome that says done; `declined`, the Outcome line as the why, for
/// blocked or failed; none for anything else.
pub(crate) fn report_outcome(report: &str) -> Option<(&'static str, Option<String>)> {
    let mut lines = report.lines();
    lines.find(|line| {
        line.trim()
            .trim_start_matches('#')
            .trim()
            .eq_ignore_ascii_case("outcome")
    })?;
    let line = lines.map(str::trim).find(|line| !line.is_empty())?;
    let word: String = line
        .trim_start_matches(['*', '_', '`', '-', ' '])
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    match word.as_str() {
        "done" => Some(("done", None)),
        "blocked" | "failed" => Some((
            "declined",
            Some(line.chars().take(WHY_CHARS).collect::<String>()),
        )),
        _ => None,
    }
}

/// Record the answer of every report (recorded since a day before `now_ms`,
/// or ever on `first`) whose delivery has none: done, or declined with the
/// Outcome line. Returns log lines.
pub(crate) fn report_answers(store: &PostgresStore, first: bool, now_ms: i64) -> Vec<String> {
    let since = if first { 0 } else { now_ms - REPORTS_BACK_MS };
    let pending = match store.report_answers(since) {
        Ok(pending) => pending,
        Err(error) => return vec![format!("ack: reports' answers: {error}")],
    };
    let mut lines = Vec::new();
    for answer in pending {
        let Some((outcome, why)) = report_outcome(&answer.report) else {
            continue;
        };
        match store.record_report_answer(answer.command_id, outcome, why.as_deref()) {
            Ok(AckRecord::Recorded { .. }) => lines.push(format!(
                "ack: task #{}'s report answers its delivery, message #{}: {outcome}",
                answer.task, answer.command_id
            )),
            Ok(_) => {}
            Err(error) => lines.push(format!(
                "ack: task #{}'s report, message #{}: {error}",
                answer.task, answer.command_id
            )),
        }
    }
    lines
}

/// Whether this process has looked at every report for an unanswered
/// delivery once ([`report_answers`]); later passes look at the last day's.
static REPORTS_ANSWERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One dispatcher pass's answers: the acks waiting in the workspaces under
/// `root` ([`collect`]; none without a root) and the reports' answers to
/// their deliveries ([`report_answers`]), each recorded once. Log lines.
pub(crate) fn answers(
    store: &PostgresStore,
    rows: &[Facts],
    root: Option<&Path>,
    now_ms: i64,
) -> Vec<String> {
    let mut lines = root.map_or_else(Vec::new, |root| collect(store, rows, root));
    let first = !REPORTS_ANSWERED.swap(true, std::sync::atomic::Ordering::Relaxed);
    lines.extend(report_answers(store, first, now_ms));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ')
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn an_ack_is_an_id_an_outcome_and_for_a_decline_a_why() {
        assert_eq!(
            parse(&words("12 done")),
            Ok(Ack {
                command_id: 12,
                outcome: "done".into(),
                why: None
            })
        );
        assert_eq!(
            parse(&words("12 done all of it")).map(|ack| ack.why),
            Ok(Some("all of it".into()))
        );
        assert_eq!(
            parse(&words("7 declined busy elsewhere")),
            Ok(Ack {
                command_id: 7,
                outcome: "declined".into(),
                why: Some("busy elsewhere".into())
            })
        );
        for bad in [
            "",
            "done",
            "x1 done",
            "0 done",
            "-3 done",
            "007 done",
            "12",
            "12 maybe",
            "12 declined",
            "12 Done",
        ] {
            assert!(parse(&words(bad)).is_err(), "{bad:?} is refused");
        }
    }

    #[test]
    fn the_workspace_is_the_panes_never_the_current_directory() {
        let root = std::env::temp_dir().join(format!("spawn-ack-{}", std::process::id()));
        let workspace = root.join("agent_worker_a_1");
        std::fs::create_dir_all(workspace.join("sub")).expect("a workspace");
        let given = |pairs: Vec<(&'static str, String)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let pane = vec![
            ("SPAWN_AGENT_ID", "agent_worker_a_1".to_string()),
            ("SPAWN_EXECUTION_ID", "exec_1".to_string()),
            ("SPAWN_WORKSPACE", workspace.display().to_string()),
        ];
        assert_eq!(pane_workspace(&given(pane.clone())), Ok(workspace.clone()));
        // Outside a pane, or a workspace that is not the agent's: refused.
        assert!(pane_workspace(&given(Vec::new())).is_err());
        let mut other = pane.clone();
        other[0].1 = "agent_worker_b_2".into();
        assert!(pane_workspace(&given(other)).is_err());
        let mut none = pane.clone();
        none.pop();
        assert!(pane_workspace(&given(none)).is_err());
        let mut missing = pane;
        missing[2].1 = root.join("agent_worker_gone").display().to_string();
        assert!(pane_workspace(&given(missing)).is_err());

        // Written whole, where the loop lists it.
        let ack = parse(&words("5 declined not now")).expect("an ack");
        let written = write(&workspace, &ack).expect("written");
        let names = agent_files::list(&workspace, &TreePath::from(ACKS)).expect("listed");
        assert_eq!(names.len(), 1, "no hidden file is left");
        assert_eq!(written.parent(), Some(workspace.join(ACKS).as_path()));
        let read: Ack =
            serde_json::from_slice(&std::fs::read(&written).expect("read")).expect("an ack");
        assert_eq!(read, ack);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The loop reads each agent's acks from its own workspace: one of its
    /// own message is recorded, and the file goes; one of another agent's
    /// message, even written in that agent's name, records nothing; a
    /// second records nothing.
    #[test]
    fn the_loop_records_an_ack_from_the_workspace_that_holds_it() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let schema = spawn_postgres::testing::TestSchema::new("acks");
        let store = PostgresStore::connect_in_schema(&url, &schema).expect("connect and migrate");
        let root =
            std::env::temp_dir().join(format!("spawn-acks-{}-{nanos:x}", std::process::id()));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let separator = if url.contains('?') { '&' } else { '?' };
            let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
            let mut client =
                postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
            let mut submit = |agent: &str, key: &str| -> i64 {
                client
                    .query_one(
                        "insert into spawn_commands (principal, idempotency_key, agent_id,
                                                     session_id, action, state, succeeded,
                                                     created_at_ms, finished_at_ms)
                         values ('bb', $1, $2, replace($2, 'agent_', 'session_'),
                                 '{\"type\":\"input\",\"text\":\"x\"}', 'finished', true,
                                 spawn_now_ms(), spawn_now_ms())
                         returning id",
                        &[&key, &agent],
                    )
                    .expect("a command")
                    .get(0)
            };
            let mine = submit("agent_worker_a_1", "bb-a");
            let theirs = submit("agent_worker_b_2", "bb-b");
            let a = root.join("agent_worker_a_1");
            std::fs::create_dir_all(&a).expect("a's workspace");
            for ack in [
                format!("{mine} done"),
                format!("{theirs} done"),
                format!("{mine} declined too late"),
            ] {
                write(&a, &parse(&words(&ack)).expect("an ack")).expect("written");
            }
            let rows: Vec<Facts> = serde_json::from_value(serde_json::json!([
                {"session_id": "session_worker_a_1", "agent_id": "agent_worker_a_1"},
                {"session_id": "session_worker_b_2", "agent_id": "agent_worker_b_2"},
            ]))
            .expect("facts");
            let lines = collect(&store, &rows, &root);
            assert_eq!(lines.len(), 3, "{lines:?}");
            assert!(lines[0].contains(&format!("message #{mine} done by agent_worker_a_1")));
            assert!(lines[1].contains("recorded nothing"), "{lines:?}");
            assert!(lines[2].contains("answered before"), "{lines:?}");
            assert_eq!(
                agent_files::list(&a, &TreePath::from(ACKS)).expect("listed"),
                Vec::<Vec<u8>>::new(),
                "every ack read is gone"
            );
            let acted: Vec<(String, serde_json::Value)> = client
                .query(
                    "select session_id, kind -> 'detail' from spawn_events
                      where event_name = 'message.acted'",
                    &[],
                )
                .expect("read")
                .iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect();
            assert_eq!(
                acted,
                vec![(
                    "session_worker_a_1".to_string(),
                    serde_json::json!({"command_id": mine, "outcome": "done", "why": null})
                )]
            );
            assert!(
                collect(&store, &rows, &root).is_empty(),
                "nothing left to read"
            );
        }));
        let _ = std::fs::remove_dir_all(&root);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn a_reports_outcome_answers_its_delivery() {
        assert_eq!(
            report_outcome("## Outcome\ndone: all of it\n## What changed\n"),
            Some(("done", None))
        );
        assert_eq!(
            report_outcome("## Outcome\n\n**Done** in one line\n"),
            Some(("done", None))
        );
        assert_eq!(
            report_outcome("## Outcome\nblocked: bb has no access to the data\n"),
            Some((
                "declined",
                Some("blocked: bb has no access to the data".into())
            ))
        );
        assert_eq!(
            report_outcome("## Outcome\nfailed — the build broke\n").map(|(outcome, _)| outcome),
            Some("declined")
        );
        assert_eq!(report_outcome("## Outcome\npartly\n"), None);
        assert_eq!(report_outcome("no outcome at all"), None);
    }
}
