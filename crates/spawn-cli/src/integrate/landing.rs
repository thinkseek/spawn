//! Moving main: whether main moved under a judging, the landing's
//! candidate, message and merged tree, the landing commit, landing a
//! task's work and a held landing, and reading main for a landing. Moved
//! here from integrate.rs unchanged (S3 of the split). integrate.rs
//! re-exports what it and its test modules use, so every name resolves
//! inside `integrate` as it did.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

use super::{Mission, Task, git, inputs, record, source_head, stop_worker};
use crate::sources;

/// The main of `source` now, when it is no longer `main` (the base a gate
/// judged): one `ls-remote`, as the landing reads it ([`source_head`]).
/// `None` when main is still `main`. The read's error when it fails: then
/// the verdict is recorded as it was found, as before this check, with the
/// error as its `main_read` (main was never checked). A source that cannot
/// be read leaves no task unrecorded, and is no reason to judge again.
pub(super) fn main_moved(source: &str, main: &str) -> Result<Option<String>, String> {
    let (_, now) = source_head(source)?;
    Ok((now != main).then_some(now))
}

/// The `stale` record of a gate whose base `main` is no longer main (`now`):
/// what it `found` (the verdict it would have been) and that verdict's
/// reason, never counted. Supply PA's mission 6 (2026-11-09): a gate started
/// on ab21a25, main moved to 513a34e at 09:00:35, and at 09:20:54 its host
/// fault was capped as the third abort in a row and halted the mission.
pub(super) fn stale_on_main_move(main: &str, now: &str, found: &str, reason: &str) -> Value {
    let said = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({})", crate::deliver::one_line(reason, 300))
    };
    json!({"type": "task.gated", "verdict": "stale", "base": main, "main_now": now,
    "found": found,
    "reason": format!(
        "main moved from {} to {} while the gate ran: its {found}{said} is no verdict on \
         the new main; judged again",
        short_commit(main),
        short_commit(now)
    )})
}

/// A commit's first 12 characters, as the integrator's messages name them.
pub(super) fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The commit that lands `branch` (the agent's pushed head, in the mirror)
/// on `main`, with `message` ([`landing_message`]), or `None` when the two
/// conflict. Always one new merge commit: first parent `main` (the base the
/// gate judges against), second parent `branch`, so main's first-parent line
/// is one commit per landing and every checkpoint stays reachable through
/// the second parents. Its tree is the branch's when main is in it, and the
/// merge of both by main's merge rules otherwise. Never the branch head
/// itself, so a head that names a large-file manifest lands without it.
/// Only objects are written, never a ref: the agent's `refs/heads/<agent>`
/// stays the Bridge's, and `spawn/<agent>` catches up with a landing through
/// the Bridge's next merge of main, which its pusher pushes like any
/// checkpoint.
pub(super) fn landing_candidate(
    mirror_path: &str,
    main: &str,
    branch: &str,
    message: &str,
) -> Result<Option<String>, String> {
    let main_is_in = git(&[
        "--git-dir",
        mirror_path,
        "merge-base",
        "--is-ancestor",
        main,
        branch,
    ])
    .is_ok();
    let tree = if main_is_in {
        git(&[
            "--git-dir",
            mirror_path,
            "rev-parse",
            &format!("{branch}^{{tree}}"),
        ])?
    } else {
        // Main moved since the worker started: land the merge of both, by
        // main's merge rules (a bare repository reads no in-tree
        // .gitattributes otherwise), as main is merged into agents' branches.
        let output = sources::controller_git()
            .args([
                "--git-dir",
                mirror_path,
                &format!("--attr-source={main}"),
                "merge-tree",
                "--write-tree",
                main,
                branch,
            ])
            .output()
            .map_err(|error| format!("run git merge-tree: {error}"))?;
        let Some(tree) = merged_tree(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )?
        else {
            return Ok(None);
        };
        tree
    };
    commit_landing(
        mirror_path,
        &[&tree, "-p", main, "-p", branch, "-m", message],
    )
    .map(Some)
}

/// Characters of a report's summary a landing's message holds at most.
pub(super) const SUMMARY_CHARS: usize = 3000;

/// Characters of a brief's first line a landing's first line holds at most.
pub(super) const TITLE_CHARS: usize = 100;

/// The message of `task`'s landing of `head`: what landed, from the record
/// alone (never a workspace). The first line holds the objective's title
/// (the brief's first line, `Objective <id>: <title>`) and names the
/// objective and the task, or, for a task without an objective, the brief's
/// first non-empty line and the task. After a blank line, the report's
/// `## What changed` section (the whole report when it has none), bounded,
/// with every line that could read as a `Spawn-` trailer indented. Last, the
/// integrator's own trailers.
pub(crate) fn landing_message(task: &Task, head: &str) -> String {
    let first = task
        .brief
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut subject = match task.objective {
        Some(objective) => {
            let title = first
                .strip_prefix(&format!("Objective {objective}:"))
                .map_or(first, str::trim);
            let title = cut(title, TITLE_CHARS);
            if title.is_empty() {
                format!("Land objective {objective}, task {}", task.id)
            } else {
                format!("{title} (objective {objective}, task {})", task.id)
            }
        }
        None if first.is_empty() => format!("Land task {}", task.id),
        None => format!("{} (task {})", cut(first, TITLE_CHARS), task.id),
    };
    if subject.to_ascii_lowercase().starts_with("spawn checkpoint") {
        subject = format!("Land {subject}");
    }
    let mut message = subject;
    message.push_str("\n\n");
    let summary = report_summary(task.report.as_deref().unwrap_or_default());
    if summary.is_empty() {
        message.push_str("The task's report is empty.");
    } else {
        for line in summary.lines() {
            if line.trim_start().to_ascii_lowercase().starts_with("spawn-") {
                message.push_str("    ");
            }
            message.push_str(line);
            message.push('\n');
        }
    }
    let message = message.trim_end().to_string();
    let mut trailers = format!("Spawn-Task: {}\n", task.id);
    if let Some(objective) = task.objective {
        let _ = writeln!(trailers, "Spawn-Objective: {objective}");
    }
    let _ = writeln!(trailers, "Spawn-Head: {head}");
    format!("{message}\n\n{trailers}")
}

/// What a landing's message quotes of `report`: its `## What changed`
/// section, or the whole report when it has none (or an empty one),
/// trimmed and cut to [`SUMMARY_CHARS`].
fn report_summary(report: &str) -> String {
    fn heading(line: &str) -> Option<&str> {
        let level = line.bytes().take_while(|&byte| byte == b'#').count();
        (1..=2)
            .contains(&level)
            .then(|| line[level..].trim().trim_end_matches(':').trim())
    }
    let mut lines = report.lines();
    let mut section = None;
    if lines.any(|line| heading(line).is_some_and(|text| text.eq_ignore_ascii_case("what changed")))
    {
        let body: Vec<&str> = lines.take_while(|line| heading(line).is_none()).collect();
        let body = body.join("\n");
        if !body.trim().is_empty() {
            section = Some(body);
        }
    }
    let text = section.as_deref().unwrap_or(report).trim();
    let summary = cut(text, SUMMARY_CHARS);
    if summary.len() < text.len() {
        format!("{summary}\n[the report continues]")
    } else {
        summary.to_string()
    }
}

/// `text` cut to at most `limit` characters, on a character boundary.
pub(super) fn cut(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(at, _)| text[..at].trim_end())
}

/// What `git merge-tree --write-tree` said, from its exit code (`None`: no
/// code, killed by a signal) and output: `Ok(Some(tree))` for a clean merge,
/// `Ok(None)` for a conflict (exit 1, the only code git gives one), and any
/// other failure an error with git's own message. A git that cannot run the
/// merge (an unknown option, a crash) says nothing about the work: it must
/// reach the operator, never send the worker after conflicts that do not
/// exist.
pub(super) fn merged_tree(
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<Option<String>, String> {
    match code {
        Some(0) => Ok(Some(
            stdout.lines().next().unwrap_or_default().trim().to_string(),
        )),
        Some(1) => Ok(None),
        failure => {
            let how = failure.map_or_else(
                || "was killed by a signal".to_string(),
                |code| format!("exited {code}"),
            );
            Err(format!(
                "git merge-tree {how}, so the landing could not be merged (this is not a \
                 conflict; Spawn needs git {}.{} or later): {}",
                crate::config::MIN_GIT.0,
                crate::config::MIN_GIT.1,
                stderr.trim()
            ))
        }
    }
}

/// Name and email of the author and committer of every landing commit.
pub(super) const LANDING_IDENTITY: (&str, &str) = ("spawn-integrate", "integrate@localhost");

/// `git commit-tree <arguments>` in the mirror, authored and committed as
/// [`LANDING_IDENTITY`] and unsigned: a landing never depends on the host's
/// git configuration (no identity in `~/.gitconfig`, another one there or in
/// the environment, `commit.gpgSign`). The identity goes in the environment,
/// which git takes over any configuration.
pub(super) fn commit_landing(mirror_path: &str, arguments: &[&str]) -> Result<String, String> {
    let (name, email) = LANDING_IDENTITY;
    let output = sources::controller_git()
        .args(["--git-dir", mirror_path, "commit-tree", "--no-gpg-sign"])
        .args(arguments)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .output()
        .map_err(|error| format!("run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git commit-tree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// What [`land`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Landing {
    /// Recorded `task.landed`: the push moved main, or the candidate was
    /// already under it.
    Landed,
    /// Main moved past the task's base without it (or it had none): gated
    /// `stale`, judged again.
    Stale,
    /// No longer landing (its mission was dropped).
    Gone,
    /// Its source is held (the hold's event id): nothing was pushed, and the
    /// task waits behind the hold, `task.held` recorded once per hold.
    Held(i64),
}

/// Move the source's default branch from the task's base to its candidate,
/// only if it is still at the base, and record the landing. A branch that
/// moved meanwhile sends the task back to be judged again, unless the
/// candidate is already in its history: a push of an earlier pass that was
/// never recorded (the integrator stopped in between), with main moved on
/// top of it since. That landing happened, and is recorded so.
///
/// The owner's hold on the source is read from the store under the landing
/// lock, before any push, never from memory: on a held source nothing is
/// pushed. A candidate already under main landed before the hold and is
/// recorded `task.landed`; any other records `task.held` (once per task and
/// hold) and stays `landing`. So a hold never splits a push from its record.
pub(super) fn land(
    store: &PostgresStore,
    mirror: &Path,
    source: &str,
    task: &Task,
) -> Result<Landing, String> {
    let (Some(base), Some(candidate)) = (task.base.as_deref(), task.candidate.as_deref()) else {
        return record(
            store,
            &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                    "revision": task.revisions,
                    "inputs": inputs::known(task.base.as_deref(), task.candidate.as_deref())}),
        )
        .map(|()| Landing::Stale);
    };
    // Held until the landing is recorded: dropping the mission waits for it,
    // and so does an owner's hold (an owner's landing takes the lock on a
    // connection of its own).
    let Some(_lock) = store
        .landing_hold(task.id)
        .map_err(|error| error.to_string())?
    else {
        println!(
            "integrate: task {} is no longer landing (its mission was dropped)",
            task.id
        );
        return Ok(Landing::Gone);
    };
    // The owner's hold, read now, under the lock: no push on a held source.
    if let Some(hold) = store
        .active_hold(source)
        .map_err(|error| error.to_string())?
    {
        return land_held(store, mirror, source, task, (base, candidate), hold);
    }
    let mirror_path = mirror.to_string_lossy().to_string();
    let (branch, _) = source_head(source)?;
    let pushed = git(&[
        "--git-dir",
        &mirror_path,
        "push",
        "--quiet",
        &format!("--force-with-lease={branch}:{base}"),
        "--",
        source,
        &format!("{candidate}:{branch}"),
    ]);
    // A push that failed may still have happened (a crash, a lost reply).
    let (_, now) = source_head(source)?;
    if pushed.is_err() && now != candidate {
        // Main moved on: past this candidate (it landed before), or past
        // its base without it. The fetch brings `now` into the mirror; one
        // that fails decides nothing this pass.
        sources::refresh(mirror, source)?;
        let under_main = git(&[
            "--git-dir",
            &mirror_path,
            "merge-base",
            "--is-ancestor",
            candidate,
            &now,
        ])
        .is_ok();
        if !under_main {
            println!(
                "integrate: task {}: {branch} moved to {now}; judging again",
                task.id
            );
            return record(
                store,
                &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                        "revision": task.revisions,
                        "inputs": inputs::known(Some(base), Some(candidate))}),
            )
            .map(|()| Landing::Stale);
        }
        println!(
            "integrate: task {}: {candidate} is already under {branch} ({now})",
            task.id
        );
    } else {
        let _ = git(&[
            "--git-dir",
            &mirror_path,
            "update-ref",
            "refs/heads/main",
            candidate,
        ]);
    }
    record(
        store,
        &json!({"type": "task.landed", "id": task.id, "from": base, "to": candidate}),
    )?;
    println!("integrate: task {} landed {candidate} on {source}", task.id);
    Ok(Landing::Landed)
}

/// [`land`] on a source the owner holds (`hold`, read under the landing
/// lock the caller holds): nothing is pushed. A candidate already in the
/// source's main landed before the hold, and is recorded `task.landed`; any
/// other waits behind the hold, `task.held` recorded once per task and hold.
/// A fetch that fails shows nothing under main: the task waits.
fn land_held(
    store: &PostgresStore,
    mirror: &Path,
    source: &str,
    task: &Task,
    (base, candidate): (&str, &str),
    hold: i64,
) -> Result<Landing, String> {
    let (branch, now) = source_head(source)?;
    let under_main = now == candidate
        || (sources::refresh(mirror, source).is_ok()
            && git(&[
                "--git-dir",
                &mirror.to_string_lossy(),
                "merge-base",
                "--is-ancestor",
                candidate,
                &now,
            ])
            .is_ok());
    if under_main {
        println!(
            "integrate: task {}: {candidate} is already under {branch} ({now}), from before \
             hold {hold}",
            task.id
        );
        record(
            store,
            &json!({"type": "task.landed", "id": task.id, "from": base, "to": candidate}),
        )?;
        return Ok(Landing::Landed);
    }
    if store
        .task_held(task.id, hold, source)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        println!(
            "integrate: task {} waits behind the owner's hold {hold} on {source}: nothing lands \
             there until a record ends it",
            task.id
        );
    }
    Ok(Landing::Held(hold))
}

/// What a mission's step reads of its source's main.
pub(super) enum Reading {
    /// Main, from a fetch that succeeded, where the last landing left it (or
    /// the mission has no baseline yet): the mission may be judged against it.
    Judge(String),
    /// No reading this pass: the fetch failed, or main is not at the
    /// baseline while an owner's landing on the source is pending.
    Nothing,
    /// Main moved with no record that explains it: the mission was halted.
    Halted,
}

/// Fetch `source`'s main into `mirror` and check it against `mission`'s
/// baseline. A fetch that failed leaves the mirror's last copy of main,
/// which is no reading of the source's main: no halt, no verdict by it. A
/// main not at the baseline halts the mission, unless an owner's landing on
/// the source is pending (`pending`): it may have moved main before its record
/// (an integrator stopped in between), so main says nothing until it is
/// applied or refused, and nothing is judged against it.
pub(super) fn read_main(
    store: &PostgresStore,
    mission: &Mission,
    source: &str,
    mirror: &Path,
    pending: &std::collections::HashMap<String, i64>,
) -> Result<Reading, String> {
    let main = match sources::refresh(mirror, source).and_then(|()| {
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "refs/heads/main",
        ])
    }) {
        Ok(main) => main,
        Err(error) => {
            eprintln!("integrate: mission {}: {error}", mission.id);
            return Ok(Reading::Nothing);
        }
    };
    let Some(baseline) = mission
        .baseline
        .as_deref()
        .filter(|baseline| *baseline != main)
    else {
        return Ok(Reading::Judge(main));
    };
    if let Some(request) = pending.get(source) {
        println!(
            "integrate: mission {}: the source's main is {main}, its baseline {baseline}; no \
             halt while the owner's landing (request {request}) is pending",
            mission.id
        );
        return Ok(Reading::Nothing);
    }
    // The owner holds the source: the hold says the owner is moving its
    // main, so a main away from the baseline is what the hold is for, not
    // something no record explains. Nothing is judged against it until the
    // hold ends (and `spawn mission moved` takes the new main as the
    // baseline).
    if let Some(hold) = mission.hold {
        println!(
            "integrate: mission {}: the source's main is {main}, its baseline {baseline}; no \
             halt while the owner holds the source (hold {hold})",
            mission.id
        );
        return Ok(Reading::Nothing);
    }
    halt_moved(store, mission, source, baseline, &main)?;
    Ok(Reading::Halted)
}

/// Halt `mission` because its source's main moved with no record that
/// explains it: the record names the source and both commits, so `spawn
/// mission moved` can tell this halt from any other and clear it when the
/// owner records the move (`spawn_source_moved`).
fn halt_moved(
    store: &PostgresStore,
    mission: &Mission,
    source: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    let reason = format!("the source's main is {to}, but the last landing left {from}");
    record(
        store,
        &json!({"type": "mission.halted", "id": mission.id, "reason": reason,
                "cause": "main_moved", "source": source, "from": from, "to": to}),
    )?;
    println!("integrate: mission {} HALTED: {reason}", mission.id);
    Ok(())
}

/// The gate's verdict when the objective's scenario passes at the base
/// commit (`scripts/rsi/gate.sh`'s base check, before any candidate build).
pub(crate) const SOLVED_ON_BASE: &str = "scenario already passes on the base";

/// Record that `mission`'s objective is solved on the base and end `task`
/// with it: the gate found its scenario passing at `base`, so no work of
/// the task could change anything (mission 4's task 182, objective 34, whose
/// owner batch had landed the behaviour first). The objective is a landing
/// for the assignment rule (`OBJECTIVE_LANDINGS` reads the record), and the
/// task is cancelled on that record — no rejection, no failed attempt, no
/// halt (`spawn_integrator_record` accepts this cancel and no other without
/// a yield).
pub(super) fn solved_on_base(
    store: &PostgresStore,
    mission: &Mission,
    task: &Task,
    scenario: Option<&Path>,
    base: &str,
    log: &str,
) -> Result<(), String> {
    let objective = task
        .objective
        .ok_or("an objective task without its objective")?;
    let scenario = scenario
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let solved = store
        .integrator_record(&json!({"type": "objective.solved", "id": mission.id,
                                   "objective": objective, "task": task.id,
                                   "scenario": scenario, "base": base, "log": log}))
        .map_err(|error| error.to_string())?;
    // The Stop first, as for a yield: if the cancel then fails, the next
    // pass finds the task again and records both again.
    stop_worker(store, task, "integrate-solved")?;
    let reason = format!(
        "objective {objective} is solved on the base: {scenario} passes at {base}, so task \
         #{} ends without a rejection",
        task.id
    );
    store
        .integrator_record(
            &json!({"type": "task.cancelled", "id": task.id, "solved": solved,
                                   "reason": reason}),
        )
        .map_err(|error| error.to_string())?;
    println!("integrate: mission {}: {reason}", mission.id);
    Ok(())
}

/// Why a mission on `source` can never land, if it cannot: agents' branches
/// reach only a directory on this host (the pusher's one destination since
/// the cross-host cut), so a gate on a URL would wait for a push nobody
/// makes, `stale` on every pass. `spawn_mission_configure` refuses such a
/// gate; one configured before that is halted, so a human sees it. A local
/// path that is missing now is not this: its fetch fails, and the pass
/// skips the mission.
pub(super) fn unlandable(source: &str) -> Option<String> {
    (!source.starts_with('/')).then(|| {
        format!(
            "its source {source} is remote: agents' branches reach only a directory on this \
             host, so its work never lands"
        )
    })
}

/// Another mission's task landing on `mission`'s source, if one is:
/// `(mission, task)`. Its push may have moved main before its record. A task
/// waiting behind the active hold on the source is not one
/// ([`Mission::in_flight`]).
pub(super) fn landing_elsewhere(missions: &[Mission], mission: &Mission) -> Option<(i64, i64)> {
    missions
        .iter()
        .filter(|other| other.id != mission.id && other.source == mission.source)
        .find_map(|other| other.in_flight().map(|task| (other.id, task.id)))
}
