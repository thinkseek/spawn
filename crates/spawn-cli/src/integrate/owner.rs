//! Step 0 of a pass: the owner's landings (objective 21,
//! docs/fleet-architecture.md §10.3, "The owner's landings").
//!
//! The owner lands on a mission source with one command, `spawn mission push
//! --source <source> --commit <commit>`: it puts the commit in the source at
//! `refs/spawn-owner/<commit>` (no branch moves) and records
//! `source.push_requested`. The integrator, which is never stopped for it,
//! applies every pending request (one with no outcome record naming it),
//! oldest first, before the steps of the missions on its source, so no
//! baseline check of that pass reads a main the owner moved without its
//! record. Each one, under the landing hold (the lock of
//! `spawn_landing_hold`, on a connection of its own so a judging's `land`
//! on the shared store is never granted it back, so no task landing's push
//! and its record are ever split by it):
//! - reads the source's main M (from the source, never the mirror: a
//!   judging may be using it);
//! - waits while a task landing on the source went out without its record
//!   (its candidate is in M but it is still `landing`): main says nothing
//!   until that landing's mission resolves it;
//! - moves main from M to the commit when M is in the commit's history, and
//!   otherwise to a merge commit (first parent M, second the commit, by
//!   main's merge rules, by the landing identity, its message naming the
//!   owner's landing, the request and the commit's subject), with a
//!   compare-and-swap (`update-ref <branch> <new> <M>` in the source);
//! - records, in the same hold, `source.moved` {source, from: M, to, request,
//!   by: human}, whose fold moves the baselines of the missions on the
//!   source that were at M and are not halted (the store names them).
//!
//! A conflict, a commit the source does not have, or a compare-and-swap
//! that fails twice records `source.push_refused` {request, reason}: nothing
//! moves and nothing halts. Any other failure (the source unreachable, the
//! store) records nothing: the request stays pending, its source's halts by
//! baseline are held, and the next pass applies it again. A request whose
//! commit main already holds was applied by an integrator that stopped
//! before its record: it is recorded now, from where its move began
//! ([`moved_from`]).
//!
//! It never moves main under a gate of its source
//! (design-owner-push-between-gates.md, D1): a request whose source has a
//! judging in flight waits, said once, and the pass that reaps that judging
//! applies it before any new gate starts. A judging of another source goes
//! on.
//!
//! A gated request (`--gated`) is judged before it lands: step 0 checks it
//! as above (the commit in the source, no unrecorded landing, main not
//! already holding it) and hands it to the integrator, which starts an owner
//! judging ([`super::Judge::judge_owner`]) ahead of the source's ready
//! tasks; the later requests of its source wait behind it.
//!
//! An urgent request (`--urgent`, D2), gated or not, does not wait out a
//! task's judging in flight on its source: step 0 names it to the
//! integrator, which stops that judging (`Integrator::stop_for`); the
//! judging records itself discarded by the request, and the request is
//! applied or judged at the pass that reaps it.
//!
//! A pending request is withdrawn by its owner (`spawn mission push
//! --withdraw <request>`, [`withdraw`]): `source.push_refused` with the
//! reason [`WITHDRAWN`], recorded under the landing hold, so a landing
//! either recorded its outcome first (and the withdrawal is refused) or
//! finds the request ended under its own hold and moves nothing ([`ended`]).
//! An owner judging in flight for it is stopped at the next pass
//! (`Integrator::stop_withdrawn`) and its result discarded
//! ([`Judged::Ended`]).
//!
//! An owner judging whose gate ends with no verdict for the reason the
//! request's judging before it recorded is refused, not judged a third time
//! (`Judge::no_verdict_kept`).

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::json;
use spawn_postgres::PostgresStore;

use super::{
    Mission, TITLE_CHARS, commit_landing, cut, git, merged_tree, record, source_head, unlandable,
};
use crate::sources;

/// Where the owner's commit waits in the source until it lands.
pub(crate) fn owner_ref(commit: &str) -> String {
    format!("refs/spawn-owner/{commit}")
}

/// Whether `text` is a full commit hash (SHA-1 or SHA-256), the only way a
/// request names its commit: never an option or a revision expression.
pub(crate) fn full_hash(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One pending request, from [`PostgresStore::source_pushes_pending`].
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Request {
    /// The `source.push_requested` record's event id.
    #[serde(rename = "request")]
    pub(super) id: i64,
    pub(super) source: String,
    pub(super) commit: String,
    /// `--gated`: judged by the mission's gate before it lands
    /// ([`Outcome::Judge`]).
    #[serde(default)]
    pub(super) gated: bool,
    /// `--urgent`: the judging in flight on its source is stopped and
    /// discarded for it (D2), so it is applied or judged at the next pass.
    #[serde(default)]
    pub(super) urgent: bool,
    /// The owner's reason, as given.
    #[serde(default)]
    pub(super) reason: Option<String>,
}

/// What applying one request did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Main moved (or already held the commit): `source.moved` recorded.
    Applied { from: String, to: String },
    /// `source.push_refused` recorded with this reason.
    Refused(String),
    /// Nothing done, nothing recorded: the request waits for the next pass.
    Waits(String),
    /// A gated request main does not hold yet: nothing moved, nothing
    /// recorded; the integrator judges it as a judging of its source
    /// (`Integrator::start_owner`), which lands it or records its refusal.
    Judge,
}

/// The pending requests, oldest first.
pub(super) fn pending(store: &PostgresStore) -> Result<Vec<Request>, String> {
    serde_json::from_value(
        store
            .source_pushes_pending()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("pending owner landings: {error}"))
}

/// The sources with a request pending, each with its oldest request: their
/// halts by baseline are held this pass.
pub(super) fn pending_by_source(store: &PostgresStore) -> Result<HashMap<String, i64>, String> {
    let mut held = HashMap::new();
    for request in pending(store)? {
        held.entry(request.source).or_insert(request.id);
    }
    Ok(held)
}

/// What step 0 keeps of the owner's requests from one pass to the next.
#[derive(Debug, Default)]
pub(super) struct Kept {
    /// The requests already said to wait for a judging in flight on their
    /// source, or for their backoff: each is said once.
    pub(super) said: HashSet<i64>,
    /// The gated requests whose owner judging found no verdict (the host's
    /// fault, no room, a gate that could not run): (how many in a row, the
    /// time before which it is not judged again), from the judging's thread.
    pub(super) backoff: std::sync::Arc<std::sync::Mutex<HashMap<i64, (u32, std::time::Instant)>>>,
}

/// Step 0: apply every pending request, oldest first. A request that waits
/// or fails keeps the later ones of its source for the next pass (they land
/// in order); other sources go on. A gated request is not applied here: it
/// goes to `judge`, at most one per source, for the integrator to judge
/// ahead of the source's tasks; the later requests of its source wait
/// behind it. Returns the sources with a request still pending, read again
/// after applying (a request recorded meanwhile moved nothing, but is held
/// all the same). Every move it recorded goes to `moved` as (source, new
/// main), for the base warm (`warm.rs`).
pub(super) fn apply_pending(
    store: &PostgresStore,
    moved: &mut Vec<(String, String)>,
    busy: &HashSet<String>,
    kept: &mut Kept,
    judge: &mut Vec<Request>,
    urgent: &mut Vec<Request>,
) -> Result<HashMap<String, i64>, String> {
    let requests = pending(store)?;
    kept.said
        .retain(|id| requests.iter().any(|request| request.id == *id));
    if let Ok(mut backoff) = kept.backoff.lock() {
        backoff.retain(|id, _| requests.iter().any(|request| request.id == *id));
    }
    if requests.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stuck = HashSet::new();
    for request in &requests {
        if stuck.contains(&request.source) {
            continue;
        }
        let said = format!(
            "integrate: the owner's landing of {} on {} (request {})",
            request.commit, request.source, request.id
        );
        // A judging of the source is in flight: main moves after it, never
        // under it (its verdict would be stale), at the pass that reaps it.
        if busy.contains(&request.source) {
            // Urgent (D2): that judging is stopped and discarded for it; the
            // request is applied (or judged) at the pass that reaps it.
            if request.urgent {
                urgent.push(request.clone());
            }
            if kept.said.insert(request.id) {
                println!("{said} waits for the judging in flight on its source");
            }
            stuck.insert(request.source.clone());
            continue;
        }
        // An owner judging that found no verdict is judged again after its
        // backoff, as a task's gate is.
        let backing_off = kept
            .backoff
            .lock()
            .ok()
            .and_then(|backoff| backoff.get(&request.id).copied())
            .filter(|(_, until)| *until > std::time::Instant::now());
        if let Some((faults, until)) = backing_off {
            if kept.said.insert(request.id) {
                println!(
                    "{said} is judged again in {} s (its owner judging found no verdict, {faults} \
                     in a row)",
                    until
                        .saturating_duration_since(std::time::Instant::now())
                        .as_secs()
                );
            }
            stuck.insert(request.source.clone());
            continue;
        }
        match apply(store, request) {
            Ok(Outcome::Judge) => {
                kept.said.remove(&request.id);
                judge.push(request.clone());
                stuck.insert(request.source.clone());
            }
            Ok(Outcome::Applied { from, to }) => {
                println!("{said}: main {from} -> {to}");
                if from != to {
                    moved.push((request.source.clone(), to));
                }
            }
            Ok(Outcome::Refused(reason)) => println!("{said}: refused: {reason}"),
            Ok(Outcome::Waits(why)) => {
                println!("{said} waits: {why}");
                stuck.insert(request.source.clone());
            }
            Err(error) => {
                eprintln!("{said}: {error}; applied again next pass");
                stuck.insert(request.source.clone());
            }
        }
    }
    pending_by_source(store)
}

/// Whether `ancestor` is in `commit`'s history in the source (both there).
pub(super) fn contains(source: &str, ancestor: &str, commit: &str) -> bool {
    git(&[
        "--git-dir",
        source,
        "merge-base",
        "--is-ancestor",
        ancestor,
        commit,
    ])
    .is_ok()
}

/// Apply one request (see the module's comment).
pub(super) fn apply(store: &PostgresStore, request: &Request) -> Result<Outcome, String> {
    let source = request.source.as_str();
    let refuse = |reason: String| refused(store, request, reason);
    if let Err(error) = sources::MissionSource::parse(source) {
        return refuse(error);
    }
    if let Some(reason) = unlandable(source) {
        return refuse(reason);
    }
    let commit = request.commit.as_str();
    if !full_hash(commit) {
        return refuse(format!("{commit:?} is not a full commit hash"));
    }
    // A task landing's push and its record are never split by this one. The
    // hold is this connection's own session lock (a dedicated connection:
    // on the shared store a judging's `land` is granted it again at once),
    // held until the outcome is recorded.
    let own = store.new_connection().map_err(|error| error.to_string())?;
    let _hold = own.source_push_hold().map_err(|error| error.to_string())?;
    // Withdrawn since this pass read it: nothing moves ([`withdraw`] records
    // under this hold).
    if let Some(why) = ended(&own, request.id)? {
        return Ok(Outcome::Refused(why));
    }
    if git(&[
        "--git-dir",
        source,
        "cat-file",
        "-e",
        &format!("{commit}^{{commit}}"),
    ])
    .is_err()
    {
        return refuse(format!(
            "the commit {commit} is not in the source (`spawn mission push` puts it at {})",
            owner_ref(commit)
        ));
    }
    if let Some(why) = pre_rewrite(store, source, commit)? {
        return refuse(why);
    }
    let missions: Vec<Mission> =
        serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
            .map_err(|error| format!("gated missions: {error}"))?;
    let on_source: Vec<&Mission> = missions
        .iter()
        .filter(|mission| mission.source.as_deref() == Some(source))
        .collect();
    let mut failed = String::new();
    for attempt in 0..2 {
        let (branch, main) = source_head(source)?;
        if attempt == 0
            && let Some(why) = unrecorded_landing(source, &main, &on_source)
        {
            return Ok(Outcome::Waits(why));
        }
        if contains(source, commit, &main) {
            // Main holds the commit: this request moved it before its
            // record was lost, or the commit was there already.
            let baselines: HashSet<&str> = on_source
                .iter()
                .filter(|mission| mission.halted.is_none())
                .filter_map(|mission| mission.baseline.as_deref())
                .filter(|baseline| *baseline != main)
                .collect();
            let from = moved_from(source, &main, commit, request.id, &baselines);
            return moved(store, request, &from, &main);
        }
        if request.gated {
            return Ok(Outcome::Judge);
        }
        let new = if contains(source, &main, commit) {
            commit.to_string()
        } else {
            match owner_merge(source, &main, commit, request.id) {
                Ok(Some(merge)) => merge,
                Ok(None) => {
                    return refuse(format!(
                        "the commit {commit} conflicts with main at {main} (by main's merge \
                         rules): merge main into it and push again"
                    ));
                }
                Err(error) => return refuse(error),
            }
        };
        let why = format!("spawn: the owner's landing (request {})", request.id);
        match git(&[
            "--git-dir",
            source,
            "update-ref",
            "-m",
            &why,
            &branch,
            &new,
            &main,
        ]) {
            Ok(_) => return moved(store, request, &main, &new),
            Err(error) => failed = error,
        }
    }
    refuse(format!(
        "main moved under the owner's landing twice; the compare-and-swap failed: {failed}"
    ))
}

/// Why a request for `commit` on `source` must not land after a history
/// rewrite (objective 173), if it must not: with a commit map loaded, a
/// commit the rewrite dropped (the request kept its recorded id, which the
/// map names dropped), or a commit whose history not yet in main
/// (`rev-list <commit> --not main`) holds an old commit the rewrite changed
/// or dropped: landing it would merge the history from before the rewrite
/// back into the rewritten main. A commit the rewrite kept is no such old
/// one. With no map loaded nothing is refused here.
pub(super) fn pre_rewrite(
    store: &PostgresStore,
    source: &str,
    commit: &str,
) -> Result<Option<String>, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    if store.commit_map_size().map_err(error)? == 0 {
        return Ok(None);
    }
    if store.commit_map_entry(commit).map_err(error)? == Some(None) {
        return Ok(Some(format!(
            "the commit {commit} was dropped by the history rewrite: push it again from the \
             rewritten history"
        )));
    }
    let (_, main) = source_head(source)?;
    let olds = store.commit_map_olds().map_err(error)?;
    let listed = git(&["--git-dir", source, "rev-list", commit, "--not", &main])?;
    let old: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|line| olds.contains(&line.to_ascii_lowercase()))
        .collect();
    Ok(old.first().map(|first| {
        format!(
            "the commit {commit} would bring {} commit(s) from before the history rewrite onto \
             main (the first: {first}): rebase it onto the rewritten main and push it again",
            old.len()
        )
    }))
}

/// A task landing on the source whose push went out without its record (its
/// candidate is in `main`, and it is still `landing`), said as why the
/// owner's landing waits: main says nothing until that landing's mission
/// resolves it.
fn unrecorded_landing(source: &str, main: &str, on_source: &[&Mission]) -> Option<String> {
    on_source.iter().find_map(|mission| {
        let task = mission.landing()?;
        let candidate = task.candidate.as_deref()?;
        contains(source, candidate, main).then(|| {
            format!(
                "task {} of mission {} pushed its landing to main without its record yet; it is \
                 resolved first",
                task.id, mission.id
            )
        })
    })
}

/// Record that `request` was refused, and why.
fn refused(store: &PostgresStore, request: &Request, reason: String) -> Result<Outcome, String> {
    refused_with(store, request, reason, &json!({}))
}

/// [`refused`], with `extra` on the record (an owner judging's `verdict`
/// and `log`).
pub(super) fn refused_with(
    store: &PostgresStore,
    request: &Request,
    reason: String,
    extra: &serde_json::Value,
) -> Result<Outcome, String> {
    let mut refusal =
        json!({"type": "source.push_refused", "request": request.id, "reason": reason});
    if let (Some(target), Some(extra)) = (refusal.as_object_mut(), extra.as_object()) {
        target.extend(extra.clone());
    }
    record(store, &refusal)?;
    Ok(Outcome::Refused(reason))
}

/// The reason of the refusal that withdraws a request ([`withdraw`]).
pub(crate) const WITHDRAWN: &str = "withdrawn by the owner";

/// Why the request `request` is pending no more (its refusal's reason, or
/// that it was applied), `None` while it is pending. A landing asks under
/// its landing hold, before it moves main.
pub(super) fn ended(store: &PostgresStore, request: i64) -> Result<Option<String>, String> {
    let outcome = store
        .source_push_outcome(request)
        .map_err(|error| error.to_string())?;
    Ok(outcome.map(|outcome| match outcome["reason"].as_str() {
        Some(reason) => reason.to_string(),
        None => "applied already".to_string(),
    }))
}

/// `spawn mission push --withdraw`: the pending request `request` ends
/// refused, [`WITHDRAWN`], whether it waits, backs off or is being judged;
/// its record, as `spawn mission push --wait` prints a refusal's. Recorded
/// as every refusal is, under the landing hold (a connection of its own, as
/// [`apply`]'s): a landing of the request in flight has its outcome
/// recorded before this one looks, and one that comes after finds the
/// request ended before it moves main ([`ended`]). A request that ended
/// already, or that does not exist, is an error and records nothing.
///
/// So is one whose commit the source's main already holds: its landing went
/// out and the integrator stopped before the record (the hold, a session
/// lock, went with it). Step 0 records that landing at its next pass
/// ([`moved_from`]); a refusal here would leave main holding a commit its
/// request calls refused, with no `source.moved`.
pub(crate) fn withdraw(store: &PostgresStore, request: i64) -> Result<serde_json::Value, String> {
    let own = store.new_connection().map_err(|error| error.to_string())?;
    let _hold = own.source_push_hold().map_err(|error| error.to_string())?;
    let outcome = || {
        own.source_push_outcome(request)
            .map_err(|error| error.to_string())
    };
    if let Some(outcome) = outcome()? {
        return Err(format!(
            "the owner landing request {request} ended already ({}): nothing is withdrawn",
            match outcome["reason"].as_str() {
                Some(reason) => format!("refused: {reason}"),
                None => "applied".to_string(),
            }
        ));
    }
    let Some(pending) = pending(&own)?
        .into_iter()
        .find(|pending| pending.id == request)
    else {
        return Err(format!(
            "no owner landing request {request}: nothing is withdrawn"
        ));
    };
    let (_, main) = source_head(&pending.source).map_err(|error| {
        format!(
            "the owner landing request {request}: whether main holds it already cannot be told \
             ({error}); nothing is withdrawn"
        )
    })?;
    if contains(&pending.source, &pending.commit, &main) {
        return Err(format!(
            "the owner landing request {request}: main holds it already: its landing is recorded \
             at the next pass; nothing is withdrawn"
        ));
    }
    record(
        &own,
        &json!({"type": "source.push_refused", "request": request, "reason": WITHDRAWN}),
    )?;
    outcome()?.ok_or_else(|| format!("the withdrawal of request {request} left no record"))
}

/// Record that `request` moved its source's main `from` -> `to`.
fn moved(
    store: &PostgresStore,
    request: &Request,
    from: &str,
    to: &str,
) -> Result<Outcome, String> {
    moved_with(store, request, from, to, &json!({}))
}

/// [`moved`], with `extra` on the record (an owner judging's `verdict`).
pub(super) fn moved_with(
    store: &PostgresStore,
    request: &Request,
    from: &str,
    to: &str,
    extra: &serde_json::Value,
) -> Result<Outcome, String> {
    let mut move_record = json!({"type": "source.moved", "source": request.source, "from": from,
                                 "to": to, "request": request.id, "by": "human"});
    if let (Some(target), Some(extra)) = (move_record.as_object_mut(), extra.as_object()) {
        target.extend(extra.clone());
    }
    record(store, &move_record)?;
    Ok(Outcome::Applied {
        from: from.to_string(),
        to: to.to_string(),
    })
}

/// Where the move of `request` began, when `main` already holds its
/// `commit`: the one baseline `b` of the missions on the source (not halted,
/// other than main) when main is this request's own move from it (the commit
/// itself with `b` in its history, or the owner's landing merge of the
/// commit on `b`, which names the request). The integrator stopped between
/// that move and its record, and the missions still wait at `b`. Otherwise
/// `main` itself: the commit was in main already, and nothing moves.
fn moved_from(
    source: &str,
    main: &str,
    commit: &str,
    request: i64,
    baselines: &HashSet<&str>,
) -> String {
    let mut only = baselines.iter();
    let (Some(baseline), None) = (only.next(), only.next()) else {
        return main.to_string();
    };
    let parent = |n: u8| {
        git(&[
            "--git-dir",
            source,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{main}^{n}"),
        ])
    };
    let ours = if main == commit {
        contains(source, baseline, main)
    } else {
        parent(1).as_deref() == Ok(*baseline)
            && parent(2).as_deref() == Ok(commit)
            && git(&["--git-dir", source, "log", "-1", "--format=%B", main]).is_ok_and(|message| {
                message
                    .lines()
                    .any(|line| line == format!("Spawn-Owner-Request: {request}"))
            })
    };
    if ours {
        (*baseline).to_string()
    } else {
        main.to_string()
    }
}

/// The merge that lands the owner's `commit` on `main`, written in the
/// source (objects only: no ref moves): first parent main, second the
/// commit, the tree by main's merge rules, by the landing identity. `None`
/// for a conflict; an error when git could not merge at all.
pub(super) fn owner_merge(
    source: &str,
    main: &str,
    commit: &str,
    request: i64,
) -> Result<Option<String>, String> {
    let output = sources::controller_git()
        .args([
            "--git-dir",
            source,
            &format!("--attr-source={main}"),
            "merge-tree",
            "--write-tree",
            main,
            commit,
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
    let subject = git(&["--git-dir", source, "log", "-1", "--format=%s", commit])?;
    let message = owner_message(request, commit, main, &subject);
    commit_landing(source, &[&tree, "-p", main, "-p", commit, "-m", &message]).map(Some)
}

/// The message of the merge that lands the owner's `commit` (subject
/// `subject`) on `main` for `request`.
pub(super) fn owner_message(request: i64, commit: &str, main: &str, subject: &str) -> String {
    format!(
        "Owner landing (request {request}): {}\n\n\
         The owner's commit {commit}, landed by `spawn mission push` on main at {main}, which \
         had moved since the commit's base.\n\n\
         Spawn-Owner-Request: {request}\n\
         Spawn-Owner-Commit: {commit}\n",
        cut(subject.trim(), TITLE_CHARS)
    )
}

/// What one owner judging ended with, for its log line and its backoff.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Judged {
    /// The gate accepted the landing candidate and main moved to it:
    /// `source.moved` recorded, with `verdict` `accepted`.
    Landed { from: String, to: String },
    /// `source.push_refused` recorded with this reason: a conflict, a
    /// candidate without the mission's scenarios directory, or the gate's
    /// rejection.
    Refused(String),
    /// No verdict on the owner's commit (the host's fault, an abort, no
    /// room, a gate that could not run, a source that could not be read):
    /// no outcome recorded (a gate that ran records `source.push_no_verdict`
    /// with its kept log, [`super::retained`]); judged again after the backoff.
    NoVerdict(String),
    /// Nothing recorded, judged again at the next pass with no backoff:
    /// main moved (behind the integrator's back, or the commit is in main
    /// now, which step 0 records).
    Again(String),
    /// The request ended while it was judged (its owner withdrew it,
    /// [`withdraw`]), and why: this judging's result is discarded, nothing
    /// is recorded, nothing moves, and it is not judged again.
    Ended(String),
}

/// The scenarios directory `scenarios` (pinned: the controller's) as a path
/// inside a tree that has it: its longest trailing components that name a
/// directory in `commit` (`rsi/scenarios` for a controller at
/// `…/spawn-abc/rsi/scenarios`).
fn scenarios_in(mirror: &str, commit: &str, scenarios: &std::path::Path) -> Option<String> {
    let parts: Vec<String> = scenarios
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    (0..parts.len()).find_map(|from| {
        let inside = parts[from..].join("/");
        let kind = git(&[
            "--git-dir",
            mirror,
            "cat-file",
            "-t",
            &format!("{commit}:{inside}"),
        ])
        .ok()?;
        (kind.trim() == "tree").then_some(inside)
    })
}

/// The candidate's own tree, written under `dir`, and in it the candidate's
/// copy of the mission's scenarios directory with `protected` emptied
/// (design-owner-push-between-gates.md, D3): the owner may change the
/// scenarios and the protected list, and is judged by the scenarios it
/// lands, never by those of the owner's working tree. The whole tree, so a
/// scenario that reaches its tree's other files (`../../scripts/…`) finds
/// the candidate's.
fn candidate_scenarios(
    mirror: &str,
    candidate: &str,
    inside: &str,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let archive = dir.join("candidate.tar");
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).map_err(|error| format!("{}: {error}", tree.display()))?;
    git(&[
        "--git-dir",
        mirror,
        "archive",
        "--format=tar",
        "-o",
        &archive.to_string_lossy(),
        candidate,
    ])?;
    let untar = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&tree)
        .output()
        .map_err(|error| format!("run tar: {error}"))?;
    let _ = std::fs::remove_file(&archive);
    if !untar.status.success() {
        return Err(format!(
            "tar of the candidate's tree: {}",
            String::from_utf8_lossy(&untar.stderr).trim()
        ));
    }
    let copy = tree.join(inside);
    std::fs::write(copy.join("protected"), "")
        .map_err(|error| format!("empty the copy's protected list: {error}"))?;
    Ok(copy)
}

/// What an owner judging's tree got for its `web/node_modules`
/// ([`controller_modules`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Modules {
    /// The installed controller's set, copied: the candidate's lockfile is
    /// the one it was installed from.
    Copied,
    /// The candidate's own set, installed offline from the host's store
    /// ([`install_modules`]).
    Installed,
    /// Nothing to give: the controller has no set, the candidate has no
    /// `web/`, or it tracks its own node_modules.
    Nothing,
    /// No verdict, and why: `web/` holds configuration an owner judging
    /// does not install with ([`web_config_refusal`]), or the install
    /// failed; `log` is pnpm's output when it ran.
    NoVerdict { why: String, log: Option<String> },
}

/// How the integrator installs a candidate's own `web/` dependencies, run
/// in the tree's `web/` with stdin closed: from the lockfile alone, offline
/// (no network), no package's lifecycle script and no pnpmfile (a
/// candidate's would run in pnpm even with --ignore-scripts); pnpm's
/// directories pinned inside `web/` over anything a configuration says (a
/// modulesDir or virtualStoreDir cannot write outside the tree, or into the
/// controller's set); no switching to another pnpm version; every file a
/// clone or a copy, never a hardlink, so no file of the tree shares an
/// inode with the store (a candidate's scenario cannot write into the
/// host's store).
pub(super) const INSTALL_ARGS: &[&str] = &[
    "install",
    "--frozen-lockfile",
    "--ignore-scripts",
    "--offline",
    "--ignore-pnpmfile",
    "--config.modules-dir=node_modules",
    "--config.virtual-store-dir=node_modules/.pnpm",
    "--config.manage-package-manager-versions=false",
    "--config.package-import-method=clone-or-copy",
    "--reporter=append-only",
];

/// The no-verdict of an install that missed a package in the host's store
/// (pnpm's ERR_PNPM_NO_OFFLINE_META or ERR_PNPM_NO_OFFLINE_TARBALL, under
/// whatever wrapper code pnpm prints first): the same words every time, so
/// a second judging of the same set refuses the request (P27).
pub(super) const NOT_IN_STORE: &str = "dependencies not in the store: the candidate's \
     web/pnpm-lock.yaml names packages the host's pnpm store does not hold, and an owner \
     judging installs offline; install the set with network (pnpm install in web/) and \
     request again";

/// The no-verdict of a candidate whose `web/` holds pnpm configuration
/// outside the reviewed shape ([`web_config_refusal`]): followed by what
/// was found.
pub(super) const WEB_CONFIG: &str =
    "the candidate's web/ holds pnpm configuration an owner judging does not install with";

/// The start of every other install failure's no-verdict.
const NOT_INSTALLED: &str = "the candidate's web dependencies could not be installed";

/// The installed controller's `web/node_modules`, or the candidate's own
/// set, given to the candidate's `tree` before its gate starts: git holds
/// no node_modules, and the scenarios of an owner judging are the
/// candidate's own, so a check that builds the UI (Spawn's check-web.sh,
/// `$controller/web/node_modules`) found none in the tree and failed on the
/// candidate and on the base alike: a host fault, retried every backoff,
/// never judged (lead-mac, 2026-11-10). The gate binds that directory into
/// its sandbox only when it is there at its setup, so it is made here,
/// with the tree, on every judging, before the room check.
///
/// First the candidate's tree is read ([`web_config_refusal`]): anything
/// outside the reviewed shape is a no-verdict and nothing is copied or
/// installed. Then:
/// - when the controller's set was installed from the candidate's
///   `web/pnpm-lock.yaml` (its `.pnpm/lock.yaml` is that file): copied, as
///   deploy/controller.sh copies it into a controller: a real directory,
///   never a symlink (a gate's tree must not reach the controller's files),
///   a reflink where the filesystem has them, else hardlinks, else a full
///   copy;
/// - otherwise the candidate's own set is installed into the tree's `web/`
///   from the host's pnpm store, offline ([`install_modules`], with `env`,
///   the gate's environment, for PATH and HOME): a failure is a
///   no-verdict (K2, requests 148307 and 148311: a changed lockfile left
///   the tree without node_modules, and no owner push that changes a
///   dependency was ever judged).
///
/// The controller's own set is never written. `Err`: the copy failed (the
/// host's).
pub(super) fn controller_modules(
    controller: &std::path::Path,
    tree: &std::path::Path,
    env: &[(String, String)],
) -> Result<Modules, String> {
    let source = controller.join("web/node_modules");
    let Ok(source) = std::fs::canonicalize(&source) else {
        return Ok(Modules::Nothing);
    };
    let web = tree.join("web");
    if std::fs::symlink_metadata(&web).is_err() || !source.is_dir() {
        return Ok(Modules::Nothing);
    }
    if let Some(found) = web_config_refusal(tree) {
        return Ok(Modules::NoVerdict {
            why: format!("{WEB_CONFIG}: {found}"),
            log: None,
        });
    }
    let target = web.join("node_modules");
    if !web.is_dir() || std::fs::symlink_metadata(&target).is_ok() {
        return Ok(Modules::Nothing);
    }
    if let Ok(installed) = std::fs::read(source.join(".pnpm/lock.yaml"))
        && std::fs::read(web.join("pnpm-lock.yaml")).ok().as_deref() != Some(&installed[..])
    {
        return Ok(match install_modules(&web, env) {
            Ok(()) => Modules::Installed,
            Err((why, log)) => {
                let _ = std::fs::remove_dir_all(&target);
                Modules::NoVerdict { why, log }
            }
        });
    }
    let temporary = web.join(".node_modules.copy");
    let _ = std::fs::remove_dir_all(&temporary);
    let copied = [&["-a", "--reflink=always"][..], &["-al"][..], &["-a"][..]]
        .iter()
        .any(|flags| {
            let done = std::process::Command::new("cp")
                .args(*flags)
                .arg(&source)
                .arg(&temporary)
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !done {
                let _ = std::fs::remove_dir_all(&temporary);
            }
            done
        });
    if !copied {
        return Err(format!(
            "copy {} into the candidate's tree failed",
            source.display()
        ));
    }
    std::fs::rename(&temporary, &target).map_err(|error| {
        let _ = std::fs::remove_dir_all(&temporary);
        format!("{}: {error}", target.display())
    })?;
    Ok(Modules::Copied)
}

/// Install the candidate's own dependency set into `web` (the tree's
/// `web/`): `pnpm` [`INSTALL_ARGS`], stdin closed, with a cleared
/// environment: PATH and HOME from `env` (the gate's environment: where
/// the integrator's unit puts pnpm, and where the store is) and LC_ALL=C,
/// nothing else (never the integrator's database URL or tokens). Then the
/// set's `node_modules/.pnpm/lock.yaml` must be the candidate's
/// `web/pnpm-lock.yaml`, byte for byte. `Err((why, log))`: the no-verdict's
/// reason, which names no path (P27 compares reasons), and pnpm's output.
fn install_modules(
    web: &std::path::Path,
    env: &[(String, String)],
) -> Result<(), (String, Option<String>)> {
    let mut command = std::process::Command::new("pnpm");
    command
        .args(INSTALL_ARGS)
        .current_dir(web)
        .env_clear()
        .stdin(std::process::Stdio::null());
    for (name, value) in env {
        if name == "PATH" || name == "HOME" {
            command.env(name, value);
        }
    }
    command.env("LC_ALL", "C");
    let output = command.output().map_err(|error| {
        (
            format!("{NOT_INSTALLED}: pnpm did not start ({:?})", error.kind()),
            None,
        )
    })?;
    let mut log = String::from_utf8_lossy(&output.stdout).to_string();
    log.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        if log.contains("ERR_PNPM_NO_OFFLINE") {
            return Err((NOT_IN_STORE.to_string(), Some(log)));
        }
        let code = first_pnpm_code(&log).map_or_else(
            || {
                output.status.code().map_or_else(
                    || "killed by a signal".to_string(),
                    |code| format!("exit {code}"),
                )
            },
            str::to_string,
        );
        return Err((
            format!("{NOT_INSTALLED}: pnpm install --frozen-lockfile --offline failed ({code})"),
            Some(log),
        ));
    }
    let installed = std::fs::read(web.join("node_modules/.pnpm/lock.yaml")).ok();
    if installed.is_none() || installed != std::fs::read(web.join("pnpm-lock.yaml")).ok() {
        return Err((
            format!(
                "{NOT_INSTALLED}: the installed set's node_modules/.pnpm/lock.yaml is not its \
                 web/pnpm-lock.yaml"
            ),
            Some(log),
        ));
    }
    Ok(())
}

/// pnpm's first error code in `log` (`ERR_PNPM_<CAPITALS>`).
fn first_pnpm_code(log: &str) -> Option<&str> {
    let at = log.find("ERR_PNPM_")?;
    let rest = &log[at..];
    let end = rest
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// What in the candidate's `tree` lies outside the shape an owner judging
/// installs with, read before anything is copied or installed (rev-g's F1,
/// F2 and F3 on R1-R3); `None` when nothing does. The rule reads text and
/// parses no YAML; a key or field is named only when it is a plain word.
/// Widening a list is the owner's change. Its second copy is
/// scripts/web-install.sh --strict=web (objective 134), which the owner's
/// own installs run: a change to one is a change to both.
/// - `web` a symbolic link (the copy or the install would go where it
///   points);
/// - an `.npmrc` or a pnpmfile in `web/`, or an `.npmrc`, a pnpmfile or a
///   `pnpm-workspace.yaml` at the tree's root;
/// - `web/package.json`, `pnpm-lock.yaml` or `pnpm-workspace.yaml` that is
///   not a regular file;
/// - the workspace file's keys ([`workspace_refusal`]), the package's
///   fields and dependencies ([`package_refusal`]) and the lockfile's
///   resolutions and values ([`lock_refusal`]).
pub(super) fn web_config_refusal(tree: &std::path::Path) -> Option<String> {
    let web = tree.join("web");
    if std::fs::symlink_metadata(&web).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Some("web is a symbolic link".to_string());
    }
    for name in [".npmrc", ".pnpmfile.cjs", ".pnpmfile.mjs", "pnpmfile.cjs"] {
        if std::fs::symlink_metadata(web.join(name)).is_ok() {
            return Some(format!("web has {name}"));
        }
    }
    for name in [
        ".npmrc",
        ".pnpmfile.cjs",
        ".pnpmfile.mjs",
        "pnpmfile.cjs",
        "pnpm-workspace.yaml",
    ] {
        if std::fs::symlink_metadata(tree.join(name)).is_ok() {
            return Some(format!("the tree's root has {name}"));
        }
    }
    let checks: [(&str, Refusal); 3] = [
        ("pnpm-workspace.yaml", workspace_refusal),
        ("package.json", package_refusal),
        ("pnpm-lock.yaml", lock_refusal),
    ];
    for (name, refusal) in checks {
        let path = web.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            return Some(format!("web/{name} is not a regular file"));
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Some(format!("web/{name} is not UTF-8 text"));
        };
        if let Some(found) = refusal(&text) {
            return Some(format!("web/{name} {found}"));
        }
    }
    None
}

/// What a file of `web/` holds outside its shape, from its text.
type Refusal = fn(&str) -> Option<String>;

/// The one top-level key main's `web/pnpm-workspace.yaml` holds.
const WORKSPACE_KEY: &str = "minimumReleaseAgeExclude";

/// A plain word: what a refusal may name (letters, digits and `-_.@/`, at
/// most 80 characters, not starting with `.` or `/`).
fn plain(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 80
        && !word.starts_with(['.', '/'])
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '/'))
}

/// What in `web/pnpm-workspace.yaml` lies outside main's shape: a top-level
/// key other than [`WORKSPACE_KEY`] (pnpmfile, modulesDir,
/// virtualStoreDir, storeDir, configDependencies, hooks and every key pnpm
/// adds later), a top-level line that is no plain key, a first setting
/// that is indented, or a tab indent.
pub(super) fn workspace_refusal(text: &str) -> Option<String> {
    let mut seen_key = false;
    for line in text.lines() {
        let body = line.trim_start_matches([' ', '\t']);
        let indent = &line[..line.len() - body.len()];
        if indent.contains('\t') {
            return Some("has a tab indent".to_string());
        }
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if !indent.is_empty() {
            if !seen_key {
                return Some("has an indented first setting".to_string());
            }
            continue;
        }
        let key = body.split_once(':').map(|(key, _)| key);
        match key {
            Some(WORKSPACE_KEY) => seen_key = true,
            Some(key) if plain(key) && !key.contains(['@', '/', '.']) => {
                return Some(format!("sets {key}"));
            }
            _ => return Some("has a top-level line that is no plain key".to_string()),
        }
    }
    None
}

/// The fields `web/package.json` may hold.
const PACKAGE_FIELDS: &[&str] = &[
    "name",
    "version",
    "private",
    "type",
    "description",
    "license",
    "scripts",
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// The fields of `web/package.json` that name dependencies.
const DEPENDENCY_FIELDS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// What in `web/package.json` lies outside the shape: a field outside
/// [`PACKAGE_FIELDS`] (so no pnpm, packageManager, devEngines or engines),
/// a dependency field that is not an object, or a dependency that is not
/// the registry's: a range, version or dist-tag alone, so any `:` or `/`
/// is refused (file:, link:, workspace:, portal:, git, github:, http(s):,
/// an npm: alias, a path), and so are a backslash, a leading `.` (a path:
/// `.`, `..`) and a tarball's name (rev-g, F2 on R2: `"x": "file:<a host
/// directory>"` copied that directory into the tree).
pub(super) fn package_refusal(text: &str) -> Option<String> {
    let Ok(serde_json::Value::Object(package)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Some("is not a JSON object".to_string());
    };
    for (field, value) in &package {
        if !PACKAGE_FIELDS.contains(&field.as_str()) {
            return Some(if plain(field) {
                format!("has the field {field}")
            } else {
                "has a field outside the reviewed ones".to_string()
            });
        }
        if !DEPENDENCY_FIELDS.contains(&field.as_str()) {
            continue;
        }
        let Some(dependencies) = value.as_object() else {
            return Some(format!("has {field} that is not an object"));
        };
        for (name, spec) in dependencies {
            if !spec.as_str().is_some_and(registry_range) {
                return Some(if plain(name) {
                    format!("has a dependency that is not a registry range ({name})")
                } else {
                    "has a dependency that is not a registry range".to_string()
                });
            }
        }
    }
    None
}

/// A registry range, version or dist-tag alone.
fn registry_range(spec: &str) -> bool {
    let lower = spec.trim_end().to_ascii_lowercase();
    !spec.contains([':', '/', '\\'])
        && !spec.trim_start().starts_with('.')
        && !TARBALLS.iter().any(|tarball| lower.ends_with(tarball))
}

/// The endings of a tarball's name, which pnpm reads as a file.
const TARBALLS: &[&str] = &[".tgz", ".tar", ".tar.gz"];

/// The schemes a lockfile value of a registry's package never starts with.
const LOCK_SCHEMES: &[&str] = &[
    "file:",
    "link:",
    "portal:",
    "workspace:",
    "git:",
    "git+",
    "github:",
    "gitlab:",
    "bitbucket:",
    "http:",
    "https:",
    "npm:",
];

/// What in `web/pnpm-lock.yaml` lies outside a registry's lockfile, read as
/// text (a lockfile can say `version: link:<dir>` under a registry
/// specifier): a backslash or a double quote anywhere (rev-g, F3 on R3: a
/// double-quoted scalar spells any key or value with escapes; pnpm writes
/// neither); a complex key, merge key, anchor, alias or tag
/// ([`hides_a_key`]); a `resolution` that is not `resolution: {integrity:
/// …}` alone, however the key is spelled ([`names_resolution`]); a value
/// that starts with one of [`LOCK_SCHEMES`], or such a scheme after a
/// package key's `@` ([`scheme_at`]).
pub(super) fn lock_refusal(text: &str) -> Option<String> {
    if text.contains(['\\', '"']) {
        return Some("has a backslash or a double quote".to_string());
    }
    for line in text.lines() {
        let body = line.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if let Some(what) = hides_a_key(body) {
            return Some(format!("has {what}"));
        }
        if names_resolution(body) && !integrity_alone(body) {
            return Some("has a resolution that is not an integrity alone".to_string());
        }
        if let Some(found) = scheme_at(body) {
            return Some(found);
        }
    }
    None
}

/// Where a scalar of `body` (a line, its indent trimmed) may start: the
/// line's start, after `- `, after `: `, after `{` or `[`, and after `, `.
fn scalar_starts(body: &str) -> Vec<usize> {
    let mut starts = vec![0];
    let bytes = body.as_bytes();
    for at in 0..bytes.len() {
        let next = match bytes[at] {
            b'{' | b'[' => at + 1,
            b':' | b',' | b'-' if bytes.get(at + 1) == Some(&b' ') => {
                if bytes[at] == b'-' && at != 0 {
                    continue;
                }
                at + 2
            }
            _ => continue,
        };
        let mut start = next;
        while bytes.get(start) == Some(&b' ') {
            start += 1;
        }
        if start < bytes.len() {
            starts.push(start);
        }
    }
    starts
}

/// A complex key (`?`), a merge key (`<<`), an anchor (`&`), an alias
/// (`*`) or a tag (`!`) in `body`, each of which can stand for a key or
/// value no text rule reads.
fn hides_a_key(body: &str) -> Option<&'static str> {
    if body.starts_with('?') {
        return Some("a complex key");
    }
    for start in scalar_starts(body) {
        let rest = &body[start..];
        if rest.starts_with("<<") {
            return Some("a merge key");
        }
        match rest.chars().next() {
            Some('&') => return Some("an anchor"),
            Some('*') => return Some("an alias"),
            Some('!') => return Some("a tag"),
            _ => {}
        }
    }
    None
}

/// Whether `body` names a `resolution` key, however it is spelled (plain,
/// single-quoted, or inside a flow mapping).
fn names_resolution(body: &str) -> bool {
    let unquoted = body.replace('\'', "");
    scalar_starts(&unquoted).into_iter().any(|start| {
        unquoted[start..]
            .strip_prefix("resolution")
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

/// `resolution: {integrity: <base64>}` and nothing else.
fn integrity_alone(body: &str) -> bool {
    body.trim_end()
        .strip_prefix("resolution: {integrity: ")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|hash| {
            !hash.is_empty()
                && hash
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-'))
        })
}

/// A value of `body` that starts with one of [`LOCK_SCHEMES`] (single
/// quotes aside), or such a scheme after an `@` (a package key's
/// version): what was found, naming the line's key when it is a plain word.
fn scheme_at(body: &str) -> Option<String> {
    let unquoted = body.replace('\'', "");
    let schemed = |text: &str| {
        let lower = text.to_ascii_lowercase();
        LOCK_SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
    };
    let found = scalar_starts(&unquoted)
        .into_iter()
        .any(|start| schemed(&unquoted[start..]))
        || unquoted
            .match_indices('@')
            .any(|(at, _)| schemed(&unquoted[at + 1..]));
    if !found {
        return None;
    }
    let key = unquoted
        .trim_start_matches("- ")
        .split_once(':')
        .map(|(key, _)| key.trim())
        .filter(|key| plain(key) && !key.contains(['@', '/', '.']));
    Some(match key {
        Some(key) => format!("has a value that is not the registry's ({key})"),
        None => "has a value that is not the registry's".to_string(),
    })
}

/// The controller whose scenarios directory is `gate`, where `inside` is
/// that directory's path in the tree (`rsi/scenarios`): `gate`, resolved
/// once, without those components.
fn controller_of(gate: &std::path::Path, inside: &str) -> std::path::PathBuf {
    let mut controller = super::pinned(gate);
    for _ in inside.split('/').filter(|part| !part.is_empty()) {
        controller.pop();
    }
    controller
}

impl super::Judge {
    /// An owner judging (design-owner-push-between-gates.md): `request`, a
    /// gated owner's request on `mission`'s source, judged by the mission's
    /// gate as a judging of the source, on this thread. The candidate is
    /// the owner's commit when main is in its history, else the owner's
    /// landing merge on main (first parent main M, second the commit), built
    /// in the mirror; the base is M; the scenarios are the candidate's own
    /// ([`candidate_scenarios`]); no red check is run on the base (an
    /// owner's push is how a red main is fixed), and a check main is red on
    /// is unsolved on the base (SPAWN_GATE_RED_UNSOLVED) for the tests main
    /// fails: the regression rule holds for what M solves, so the push is
    /// judged on the rest, never held or refused for a red it did not
    /// cause, and rejected for a test it newly breaks inside that check. Accepted: main
    /// moves from M to the candidate by the same compare-and-swap push as a
    /// task's landing, under the landing hold, and `source.moved` names the
    /// verdict. Rejected: `source.push_refused` with the verdict and the
    /// log; nothing moves, and no mission counts it. Anything else, an
    /// abort included (a harness error), is no verdict
    /// ([`Judged::NoVerdict`]), judged again after its backoff.
    pub(super) fn judge_owner(
        &self,
        store: &PostgresStore,
        mirror: &std::path::Path,
        mission: &Mission,
        request: &Request,
        jobs: Option<usize>,
    ) -> Judged {
        let source = request.source.as_str();
        let commit = request.commit.as_str();
        let mirror_path = mirror.to_string_lossy().to_string();
        // Withdrawn since step 0 read it: no gate runs for it.
        if let Ok(Some(why)) = ended(store, request.id) {
            return Judged::Ended(why);
        }
        if let Err(error) = sources::refresh(mirror, source) {
            return Judged::NoVerdict(error);
        }
        let (branch, main) = match source_head(source) {
            Ok(head) => head,
            Err(error) => return Judged::NoVerdict(error),
        };
        let mirror_main = git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"]);
        if mirror_main.as_deref().map(str::trim) != Ok(main.as_str()) {
            return Judged::Again(format!("main of {source} moved while it was read"));
        }
        let reference = owner_ref(commit);
        if let Err(error) = git(&[
            "--git-dir",
            &mirror_path,
            "fetch",
            "--quiet",
            "--",
            source,
            &format!("+{reference}:{reference}"),
        ]) {
            return Judged::NoVerdict(format!("fetch {reference} from {source}: {error}"));
        }
        if contains(&mirror_path, commit, &main) {
            return Judged::Again(format!("main ({main}) holds {commit} now"));
        }
        let candidate = if contains(&mirror_path, &main, commit) {
            commit.to_string()
        } else {
            match owner_merge(&mirror_path, &main, commit, request.id) {
                Ok(Some(merge)) => merge,
                Ok(None) => {
                    return refuse_owner(
                        store,
                        request,
                        format!(
                            "the commit {commit} conflicts with main at {main} (by main's merge \
                             rules): merge main into it and push again"
                        ),
                        &json!({"verdict": "conflict"}),
                    );
                }
                Err(error) => return Judged::NoVerdict(error),
            }
        };
        let work = self.root.join("owner-gates").join(request.id.to_string());
        let judged = self.judge_owner_candidate(
            store, mirror, mission, request, jobs, &main, &candidate, &work,
        );
        let _ = std::fs::remove_dir_all(&work);
        match judged {
            Ok(verdict) => verdict,
            Err(Ok(extra)) => self.land_owner(
                store, mirror, mission, request, &branch, &main, &candidate, &extra,
            ),
            Err(Err(error)) => Judged::NoVerdict(error),
        }
    }

    /// The gate run of an owner judging: `Ok` with what it ended with when
    /// that is not an acceptance, `Err(Ok(extra))` when the gate accepted
    /// `candidate` on `main` (the fields its `source.moved` adds:
    /// [`accepted_extra`]), `Err(Err(_))` when it could not be run.
    /// The gate's environment for an owner's candidate.
    fn owner_gate_env(
        &self,
        scenarios: &std::path::Path,
        jobs: Option<usize>,
    ) -> Vec<(String, String)> {
        // A check main is red on is unsolved on the base for an owner's push
        // (the lead, 2026-11-10): the regression rule holds for what the base
        // solves, and a push during a red main must not have to go ungated.
        let mut gate_env = self.judging_env(scenarios, jobs);
        gate_env.retain(|(name, _)| name != "SPAWN_GATE_RED_UNSOLVED");
        gate_env.push(("SPAWN_GATE_RED_UNSOLVED".to_string(), "1".to_string()));
        // The owner's own candidate (gate.sh's "Three classes of a failing
        // regression"): a validator that fails on the base too does not end
        // the gate; the rest is judged and the verdict names it, carried on
        // the `source.moved` and raised as an alert (alerts.rs
        // `validator_defect`).
        gate_env.retain(|(name, _)| name != "SPAWN_GATE_OWNER_CANDIDATE");
        gate_env.push(("SPAWN_GATE_OWNER_CANDIDATE".to_string(), "1".to_string()));
        gate_env
    }

    #[allow(clippy::too_many_arguments)]
    fn judge_owner_candidate(
        &self,
        store: &PostgresStore,
        mirror: &std::path::Path,
        mission: &Mission,
        request: &Request,
        jobs: Option<usize>,
        main: &str,
        candidate: &str,
        work: &std::path::Path,
    ) -> Result<Judged, Result<serde_json::Value, String>> {
        use crate::gate::RoomFailure;
        let mirror_path = mirror.to_string_lossy().to_string();
        // A candidate without the mission's scenarios directory cannot be
        // judged by it: refused, the owner's to fix. Writing its tree out is
        // the host's: a failure there is no verdict.
        let Some(inside) =
            scenarios_in(&mirror_path, candidate, std::path::Path::new(&mission.gate))
        else {
            return Ok(refuse_owner(
                store,
                request,
                format!(
                    "the candidate {candidate} has no scenarios directory where the mission's is \
                     ({})",
                    mission.gate
                ),
                &json!({"verdict": "no_scenarios"}),
            ));
        };
        let scenarios = match candidate_scenarios(&mirror_path, candidate, &inside, work) {
            Ok(scenarios) => scenarios,
            Err(error) => return Err(Err(error)),
        };
        let controller = controller_of(std::path::Path::new(&mission.gate), &inside);
        if let Some(judged) = self.give_modules(store, request, &controller, &work.join("tree")) {
            return Ok(judged);
        }
        let test_database = super::needs_test_database(&scenarios);
        let room = if test_database {
            self.room.clone()
        } else {
            self.room.without_test_database()
        };
        match room.check(&self.gate_env) {
            Ok(_) => {}
            Err(RoomFailure::Short { short, .. }) => {
                return Ok(Judged::NoVerdict(format!(
                    "no room to judge now ({})",
                    short.check()
                )));
            }
            Err(RoomFailure::Bad(reason)) => return Err(Err(reason)),
        }
        let gate_env = self.owner_gate_env(&scenarios, jobs);
        let started = super::now_ms();
        let judged = crate::gate::judge(
            &super::pinned(&self.gate),
            mirror,
            main,
            candidate,
            &scenarios,
            None,
            &gate_env,
            super::gate_wall_clock(),
        );
        let ended = super::now_ms();
        // The gate's wall clock, and the mission whose gate judged it, on
        // the judging's record when the gate gave a verdict: the
        // scorecard's gate time counts the run (objective 061).
        let run = GateRun {
            mission: mission.id,
            gate_ms: ended.saturating_sub(started),
        };
        let refuse = |verdict: &str, reason: String, log: &str| {
            Ok(refuse_owner(
                store,
                request,
                reason,
                &json!({"verdict": verdict, "log": log, "mission": run.mission,
                        "gate_ms": run.gate_ms}),
            ))
        };
        match judged {
            Ok(verdict) if verdict.accepted => Err(Ok(accepted_extra(&verdict, run))),
            Ok(verdict)
                if test_database
                    && let Some(fault) =
                        super::host::host_fault(&verdict.reason, &verdict.output, || {
                            self.crash_in(started, ended)
                        }) =>
            {
                Ok(self.no_verdict_kept(
                    store,
                    request,
                    format!("the gate's rejection was the host's: {fault:?}"),
                    Some(&verdict.output),
                    true,
                ))
            }
            Ok(verdict) => refuse(
                "rejected",
                format!("the owner's gate rejected it: {}", verdict.reason.trim()),
                &verdict.output,
            ),
            // Reaped: the integrator that started the gate records its end.
            Err(failure @ crate::gate::GateFailure::Reaped) => {
                Ok(Judged::NoVerdict(no_verdict(failure)))
            }
            Err(failure) => {
                let (log, again) = (failure_log(&failure).map(str::to_string), repeats(&failure));
                let why = no_verdict(failure);
                Ok(self.no_verdict_kept(store, request, why, log.as_deref(), again))
            }
        }
    }

    /// The candidate's `web/node_modules`, made before the room check and
    /// the gate ([`controller_modules`], with the gate's environment): `None`
    /// to go on to the gate; `Some` when there is nothing to judge with, a
    /// no-verdict kept through [`Self::no_verdict_kept`] (pnpm's output as
    /// its log, so a second one for the same reason refuses the request,
    /// P27), or the copy failed (the host's: no verdict, judged again).
    fn give_modules(
        &self,
        store: &PostgresStore,
        request: &Request,
        controller: &std::path::Path,
        tree: &std::path::Path,
    ) -> Option<Judged> {
        match controller_modules(controller, tree, &self.gate_env) {
            Ok(Modules::Copied | Modules::Nothing) => None,
            Ok(Modules::Installed) => {
                println!(
                    "integrate: owner request {}: the candidate's web dependencies installed \
                     offline from the host's store",
                    request.id
                );
                None
            }
            Ok(Modules::NoVerdict { why, log }) => {
                println!("integrate: owner request {}: {why}", request.id);
                Some(self.no_verdict_kept(store, request, why, log.as_deref(), true))
            }
            Err(error) => Some(Judged::NoVerdict(error)),
        }
    }

    /// A gate run that gave `request` no verdict (P26): the gate's `log`,
    /// when it printed one, kept under the integrator's root
    /// ([`super::retained`]), and `source.push_no_verdict` {request, reason,
    /// log: the kept file's path or null} recorded, naming the judging. The
    /// request stays pending (no outcome names it), judged again after its
    /// backoff; the journal's line ([`judged`]) names the path too. Neither
    /// keeping nor recording failing changes that: each is said and the
    /// no-verdict stands.
    ///
    /// Unless it is the second in a row for one reason (P27): when
    /// `repeats` (a fault, not a wait for room nor a gate that never
    /// started) and the request's last `source.push_no_verdict` before this
    /// one carries the same `reason` (the record's, apart from the kept
    /// log's path, which is new at every judging), the fault is not
    /// passing, and a third judging would only hold the source behind a
    /// longer backoff (request 131083, 2026-11-11). The request is
    /// refused, `source.push_refused` with `verdict` `no_verdict` and
    /// `kept_log`, this judging's log, and its owner classifies the fault
    /// before requesting again. The store's records decide, so an
    /// integrator that restarted between the two refuses all the same.
    fn no_verdict_kept(
        &self,
        store: &PostgresStore,
        request: &Request,
        why: String,
        log: Option<&str>,
        repeats: bool,
    ) -> Judged {
        // Read before this judging's own record is written.
        let before = if repeats {
            store
                .source_push_last_no_verdict(request.id)
                .unwrap_or_else(|error| {
                    eprintln!(
                        "integrate: owner request {}: read its last no verdict: {error}",
                        request.id
                    );
                    None
                })
        } else {
            None
        };
        let judging = super::judging::current_id();
        let kept = log.and_then(|log| {
            match super::retained::keep(
                &self.root,
                request.id,
                judging.as_deref(),
                log,
                super::retained::KEEP,
            ) {
                Ok(path) => Some(path.to_string_lossy().to_string()),
                Err(error) => {
                    eprintln!(
                        "integrate: owner request {}: keep its gate's log: {error}",
                        request.id
                    );
                    None
                }
            }
        });
        let mut record =
            json!({"type": "source.push_no_verdict", "request": request.id, "reason": why});
        record["log"] = json!(kept);
        if let Err(error) = super::record(store, &record) {
            eprintln!(
                "integrate: owner request {}: record its no verdict: {error}",
                request.id
            );
        }
        // The stored reason is masked (objective 170): compare it with
        // exactly what the store keeps of this one, never the raw text.
        let stored = spawn_postgres::control_stored_text("source.push_no_verdict", "reason", &why);
        if before.as_deref() == Some(stored.as_str()) {
            return refuse_owner(
                store,
                request,
                format!(
                    "no verdict twice for the same reason ({why}): classify it (candidate \
                     failure, test defect or infrastructure) before requesting again"
                ),
                &json!({"verdict": "no_verdict", "kept_log": kept}),
            );
        }
        Judged::NoVerdict(match kept {
            Some(path) => format!("{why}; the gate's log: {path}"),
            None => why,
        })
    }

    /// Land an accepted owner judging: under the landing hold (a connection
    /// of its own, as step 0's), push `candidate` to the source's `branch`
    /// with a lease on `main`, then `source.moved` with `verdict`
    /// `accepted`. A push that did not happen (main moved behind the
    /// integrator's back) records nothing: judged again on the new main.
    #[allow(clippy::too_many_arguments)]
    fn land_owner(
        &self,
        store: &PostgresStore,
        mirror: &std::path::Path,
        mission: &Mission,
        request: &Request,
        branch: &str,
        main: &str,
        candidate: &str,
        extra: &serde_json::Value,
    ) -> Judged {
        let source = request.source.as_str();
        let mirror_path = mirror.to_string_lossy().to_string();
        let own = match store.new_connection() {
            Ok(own) => own,
            Err(error) => return Judged::NoVerdict(error.to_string()),
        };
        let _hold = match own.source_push_hold() {
            Ok(hold) => hold,
            Err(error) => return Judged::NoVerdict(error.to_string()),
        };
        // Withdrawn while its gate ran: an accepted candidate does not land
        // ([`withdraw`] records under this hold, so not after this either).
        match ended(&own, request.id) {
            Ok(None) => {}
            Ok(Some(why)) => return Judged::Ended(why),
            Err(error) => return Judged::NoVerdict(error),
        }
        let pushed = git(&[
            "--git-dir",
            &mirror_path,
            "push",
            "--quiet",
            &format!("--force-with-lease={branch}:{main}"),
            "--",
            source,
            &format!("{candidate}:{branch}"),
        ]);
        // A push that failed may still have happened (a crash, a lost reply).
        let now = match source_head(source) {
            Ok((_, now)) => now,
            Err(error) => return Judged::NoVerdict(error),
        };
        if now != candidate {
            return Judged::Again(format!(
                "main of {source} moved to {now} under the owner's landing: {}",
                pushed.err().unwrap_or_default()
            ));
        }
        let _ = git(&[
            "--git-dir",
            &mirror_path,
            "update-ref",
            "refs/heads/main",
            candidate,
        ]);
        if let Err(error) = moved_with(store, request, main, candidate, extra) {
            // Main moved: the next pass finds the commit in main and
            // records the move from where it began ([`moved_from`]).
            return Judged::Again(format!("record the move: {error}"));
        }
        if let Ok(mut landed) = self.landed.lock() {
            landed.push(super::warm::Want {
                source: source.to_string(),
                commit: candidate.to_string(),
                scenarios: std::path::PathBuf::from(&mission.gate),
            });
        }
        Judged::Landed {
            from: main.to_string(),
            to: candidate.to_string(),
        }
    }
}

/// One owner judging's gate run: the mission whose gate judged it, and the
/// gate's wall clock in ms (the scorecard's gate time, objective 061).
#[derive(Debug, Clone, Copy)]
struct GateRun {
    mission: i64,
    gate_ms: i64,
}

/// The fields an accepted owner judging's `source.moved` adds: its verdict,
/// the gate run (`mission`, `gate_ms`),
/// and the validators the gate judged it without, failing on the base too
/// (`validator_defects`), or that the owner's list kept from running
/// (`quarantined`), and the regressions that passed in it (`passed`, run;
/// `reused`, from the base's cache), when there are any.
fn accepted_extra(verdict: &crate::gate::Verdict, run: GateRun) -> serde_json::Value {
    let mut extra = json!({"verdict": "accepted", "mission": run.mission, "gate_ms": run.gate_ms});
    if !verdict.validator_defects.is_empty() {
        extra["validator_defects"] = json!(verdict.validator_defects);
    }
    if !verdict.quarantined.is_empty() {
        extra["quarantined"] = json!(verdict.quarantined);
    }
    // Which regressions passed in this gate (run, or reused from the base's
    // cache): what clears an open validator alert of the source.
    if !verdict.passed.is_empty() {
        extra["passed"] = json!(verdict.passed);
    }
    if !verdict.reused.is_empty() {
        extra["reused"] = json!(verdict.reused);
    }
    extra
}

/// Record `request`'s refusal by its owner judging.
fn refuse_owner(
    store: &PostgresStore,
    request: &Request,
    reason: String,
    extra: &serde_json::Value,
) -> Judged {
    match refused_with(store, request, reason.clone(), extra) {
        Ok(_) => Judged::Refused(reason),
        Err(error) => Judged::NoVerdict(format!("record its refusal ({reason}): {error}")),
    }
}

/// Whether a gate's end with no verdict is a fault that may be "the same
/// reason twice" ([`super::Judge::no_verdict_kept`]): not waiting for room,
/// nor a gate that never started or that another integrator reaped (which
/// records nothing anyway), which are no fault of the request however often
/// in a row.
pub(super) fn repeats(failure: &crate::gate::GateFailure) -> bool {
    use crate::gate::GateFailure;
    !matches!(
        failure,
        GateFailure::NoRoom { .. }
            | GateFailure::Failed(_)
            | GateFailure::Parameter(_)
            | GateFailure::Reaped
    )
}

/// What the gate printed, for the ends that ran it (stdout, then stderr
/// marked `! `); None for one that never ran, ran past its wall clock or was
/// reaped by another integrator.
fn failure_log(failure: &crate::gate::GateFailure) -> Option<&str> {
    use crate::gate::GateFailure;
    match failure {
        GateFailure::Aborted { log, .. }
        | GateFailure::NoRoom { log, .. }
        | GateFailure::HostFault { log, .. }
        | GateFailure::ValidatorDefect { log, .. }
        | GateFailure::MainRed { log, .. }
        | GateFailure::CannotJudge { log, .. }
        | GateFailure::BaseDisagrees { log, .. } => Some(log),
        GateFailure::Timeout
        | GateFailure::Reaped
        | GateFailure::Failed(_)
        | GateFailure::Parameter(_) => None,
    }
}

/// Why a gate that gave no verdict gave none, for an owner judging: every
/// such end is no verdict on the owner's commit, judged again after the
/// backoff (the lead, 2026-11-10). An abort is a harness error, as the
/// host's other failures are; a main red on a check (the gate's fallback:
/// an owner's gate runs with SPAWN_GATE_RED_UNSOLVED, so a red check is
/// unsolved on the base and judged no further) is no fault of the push.
fn no_verdict(failure: crate::gate::GateFailure) -> String {
    use crate::gate::GateFailure;
    match failure {
        GateFailure::Aborted { reason, .. } => format!("the owner's gate aborted: {reason}"),
        GateFailure::MainRed { scenario, .. } => format!("main is red on {scenario}"),
        GateFailure::Timeout => "the gate ran past its wall clock".to_string(),
        GateFailure::Reaped => "the integrator that started the gate reaped it".to_string(),
        GateFailure::Failed(error) => format!("the gate could not start: {error}"),
        GateFailure::Parameter(error) => format!("invalid gate parameters: {error}"),
        GateFailure::NoRoom { short, .. } => format!("no room to judge ({})", short.check()),
        GateFailure::HostFault {
            scenario, reason, ..
        } => format!("the host's fault on {scenario}: {reason}"),
        GateFailure::CannotJudge { scenario, .. } => format!("the host cannot judge {scenario}"),
        GateFailure::ValidatorDefect { defects, .. } => format!(
            "a validator defect on {} (no verdict: the gate judged it as a task's)",
            defects.join(", ")
        ),
        GateFailure::BaseDisagrees { scenario, .. } => {
            format!("the base disagreed with itself on {scenario}")
        }
    }
}

/// An owner judging's end, on its thread: said, and its backoff kept
/// (`backoff`, shared with step 0): grown after no verdict (the host-fault
/// backoff from `base`), dropped otherwise.
///
/// A request that ended while it was judged without this judging recording
/// its outcome (withdrawn, [`withdraw`]) has the judging's result discarded.
pub(super) fn judged(
    store: &PostgresStore,
    id: i64,
    said: &str,
    judged: Judged,
    backoff: &std::sync::Mutex<HashMap<i64, (u32, std::time::Instant)>>,
    base: std::time::Duration,
) {
    let judged = match judged {
        Judged::NoVerdict(_) | Judged::Again(_) if let Ok(Some(why)) = ended(store, id) => {
            Judged::Ended(why)
        }
        other => other,
    };
    let mut backoff = backoff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match judged {
        Judged::NoVerdict(why) => {
            let faults = backoff.get(&id).map_or(0, |(faults, _)| *faults) + 1;
            let wait = super::host_fault_backoff(base, faults);
            backoff.insert(id, (faults, std::time::Instant::now() + wait));
            println!(
                "{said}: no verdict ({why}); judged again in {} s",
                wait.as_secs()
            );
        }
        Judged::Again(why) => {
            backoff.remove(&id);
            println!("{said}: judged again: {why}");
        }
        Judged::Refused(reason) => {
            backoff.remove(&id);
            println!("{said}: refused: {reason}");
        }
        Judged::Ended(why) => {
            backoff.remove(&id);
            println!("{said}: its judging is discarded, the request ended: {why}");
        }
        Judged::Landed { from, to } => {
            backoff.remove(&id);
            println!("{said}: accepted, main {from} -> {to}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{lock_refusal, package_refusal, workspace_refusal};

    /// Main's web/ files, as an owner judging reads them.
    const MAIN_WORKSPACE: &str = include_str!("../../../../web/pnpm-workspace.yaml");
    const MAIN_PACKAGE: &str = include_str!("../../../../web/package.json");
    const MAIN_LOCK: &str = include_str!("../../../../web/pnpm-lock.yaml");

    #[test]
    fn a_workspace_file_holds_mains_one_key_alone() {
        assert_eq!(workspace_refusal(MAIN_WORKSPACE), None);
        assert_eq!(workspace_refusal(""), None);
        assert_eq!(
            workspace_refusal("# a comment\nminimumReleaseAgeExclude: []\n"),
            None
        );
        for (text, found) in [
            (
                "minimumReleaseAgeExclude: []\npnpmfile: hook.cjs\n",
                "sets pnpmfile",
            ),
            ("modulesDir: ../../outside\n", "sets modulesDir"),
            ("virtualStoreDir: /tmp/v\n", "sets virtualStoreDir"),
            ("storeDir: /tmp/s\n", "sets storeDir"),
            ("configDependencies: {}\n", "sets configDependencies"),
            ("hooks: {}\n", "sets hooks"),
            (
                "'pnpmfile': hook.cjs\n",
                "has a top-level line that is no plain key",
            ),
            (
                "\"pnpmfile\": hook.cjs\n",
                "has a top-level line that is no plain key",
            ),
            (
                "{pnpmfile: hook.cjs}\n",
                "has a top-level line that is no plain key",
            ),
            ("---\n", "has a top-level line that is no plain key"),
            ("  pnpmfile: hook.cjs\n", "has an indented first setting"),
            ("minimumReleaseAgeExclude:\n\t- x\n", "has a tab indent"),
        ] {
            assert_eq!(workspace_refusal(text).as_deref(), Some(found), "{text}");
        }
    }

    #[test]
    fn a_package_json_holds_plain_fields_alone() {
        assert_eq!(package_refusal(MAIN_PACKAGE), None);
        assert_eq!(
            package_refusal(
                r#"{"name": "w", "private": true, "license": "MIT", "description": "d",
                    "dependencies": {"@hugeicons/react": "^4.4.0", "a": "1.0.0", "b": "latest",
                                     "c": ">=1 <2 || ^3", "d": "*"},
                    "peerDependencies": {"e": "~1.2"}, "optionalDependencies": {}}"#
            ),
            None,
            "registry ranges, dist-tags and scoped names"
        );
        for (text, found) in [
            (r#"{"pnpm": {}}"#, "has the field pnpm"),
            (
                r#"{"packageManager": "pnpm@9"}"#,
                "has the field packageManager",
            ),
            (r#"{"devEngines": {}}"#, "has the field devEngines"),
            (r#"{"engines": {}}"#, "has the field engines"),
            (
                r#"{"dependencies": []}"#,
                "has dependencies that is not an object",
            ),
            (
                r#"{"devDependencies": "x"}"#,
                "has devDependencies that is not an object",
            ),
            ("[]", "is not a JSON object"),
            ("{", "is not a JSON object"),
        ] {
            assert_eq!(package_refusal(text).as_deref(), Some(found), "{text}");
        }
        for spec in [
            "\"file:/home/u/.config\"",
            "\"link:../x\"",
            "\"github:o/r\"",
            "\"npm:react@19\"",
            "\"workspace:*\"",
            "\"portal:../p\"",
            "\"git+https://h/r.git\"",
            "\"https://h/x.tgz\"",
            "\"o/r\"",
            "\"../x\"",
            "\".\"",
            "\"~/x\"",
            "\"x.tgz\"",
            "1",
            "null",
        ] {
            let text = format!(r#"{{"dependencies": {{"x": {spec}}}}}"#);
            assert_eq!(
                package_refusal(&text).as_deref(),
                Some("has a dependency that is not a registry range (x)"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_lockfile_resolves_from_the_registry_alone() {
        assert_eq!(lock_refusal(MAIN_LOCK), None);
        let good = "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  \
                    excludeLinksFromLockfile: false\n\nimporters:\n\n  .:\n    dependencies:\n      \
                    get-caller-file:\n        specifier: '*'\n        version: 2.0.5\n\n\
                    packages:\n\n  get-caller-file@2.0.5:\n    resolution: {integrity: \
                    sha512-abc/+=}\n    engines: {node: 6.* || 8.* || >= 10.*}\n\n  \
                    terminal-link@2.1.1:\n    resolution: {integrity: sha512-xyz}\n\n  \
                    '@types/react@19.0.0':\n    resolution: {integrity: sha512-q}\n\n\
                    snapshots:\n\n  get-caller-file@2.0.5: {}\n";
        assert_eq!(lock_refusal(good), None, "file- and link-named packages");
        for (text, found) in [
            (
                "      made-up-b:\n        version: link:/home/u/.config\n",
                "has a value that is not the registry's (version)",
            ),
            (
                "        version: file:../x\n",
                "has a value that is not the registry's (version)",
            ),
            (
                "        specifier: 'npm:react@19'\n",
                "has a value that is not the registry's (specifier)",
            ),
            (
                "        version: github:o/r\n",
                "has a value that is not the registry's (version)",
            ),
            (
                "  secretpkg@file:../x:\n",
                "has a value that is not the registry's",
            ),
            (
                "  'x@link:/etc': {}\n",
                "has a value that is not the registry's",
            ),
            (
                "    resolution: {tarball: http://h/x.tgz}\n",
                "has a resolution that is not an integrity alone",
            ),
            (
                "    resolution: {directory: ../x, type: directory}\n",
                "has a resolution that is not an integrity alone",
            ),
            (
                "    resolution: {integrity: sha512-a, tarball: file:x}\n",
                "has a resolution that is not an integrity alone",
            ),
            (
                "    resolution:\n      directory: ../x\n",
                "has a resolution that is not an integrity alone",
            ),
            (
                "    'resolution': {directory: ../x}\n",
                "has a resolution that is not an integrity alone",
            ),
            (
                "    {resolution: {directory: ../x}}\n",
                "has a resolution that is not an integrity alone",
            ),
            ("    ? version\n    : link:/x\n", "has a complex key"),
            ("    <<: *base\n", "has a merge key"),
            ("    base: &base {}\n", "has an anchor"),
            ("    version: *v\n", "has an alias"),
            ("    version: !!str x\n", "has a tag"),
            (
                "        version: \"\\x6cink:/x\"\n",
                "has a backslash or a double quote",
            ),
            (
                "  \"made-up-b@\\x6cink:/x\": {}\n",
                "has a backslash or a double quote",
            ),
            (
                "    \"r\\x65solution\": {directory: ../x}\n",
                "has a backslash or a double quote",
            ),
            (
                "        version: \"1.0.0\"\n",
                "has a backslash or a double quote",
            ),
            (
                "        version: 1.0.0\\\n",
                "has a backslash or a double quote",
            ),
        ] {
            assert_eq!(lock_refusal(text).as_deref(), Some(found), "{text}");
        }
    }
}
