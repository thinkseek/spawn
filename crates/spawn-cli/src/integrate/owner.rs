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
//! record. Each one, under the landing hold ([`super::LANDING`] and the
//! store's lock of `spawn_landing_hold`, so no task landing's push and its
//! record are ever split by it):
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
//! It never waits for a gate: a judging of another source goes on, and one
//! of the same source finishes and finds main moved when it lands (`stale`,
//! judged again on the new main).

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::json;
use spawn_postgres::PostgresStore;

use super::{
    LANDING, Mission, TITLE_CHARS, commit_landing, cut, git, merged_tree, record, source_head,
    unlandable,
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

/// Step 0: apply every pending request, oldest first. A request that waits
/// or fails keeps the later ones of its source for the next pass (they land
/// in order); other sources go on. Returns the sources with a request still
/// pending, read again after applying (a request recorded meanwhile moved
/// nothing, but is held all the same).
pub(super) fn apply_pending(store: &PostgresStore) -> Result<HashMap<String, i64>, String> {
    let requests = pending(store)?;
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
        match apply(store, request) {
            Ok(Outcome::Applied { from, to }) => println!("{said}: main {from} -> {to}"),
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
fn contains(source: &str, ancestor: &str, commit: &str) -> bool {
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
    // A task landing's push and its record are never split by this one.
    let _landing = LANDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _hold = store
        .source_push_hold()
        .map_err(|error| error.to_string())?;
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
            Err(error) => failed = error.to_string(),
        }
    }
    refuse(format!(
        "main moved under the owner's landing twice; the compare-and-swap failed: {failed}"
    ))
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
    record(
        store,
        &json!({"type": "source.push_refused", "request": request.id, "reason": reason}),
    )?;
    Ok(Outcome::Refused(reason))
}

/// Record that `request` moved its source's main `from` -> `to`.
fn moved(
    store: &PostgresStore,
    request: &Request,
    from: &str,
    to: &str,
) -> Result<Outcome, String> {
    record(
        store,
        &json!({"type": "source.moved", "source": request.source, "from": from, "to": to,
                "request": request.id, "by": "human"}),
    )?;
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
    let parent =
        |n: u8| git(&["--git-dir", source, "rev-parse", "--verify", "--quiet", &format!("{main}^{n}")]);
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
fn owner_merge(
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
