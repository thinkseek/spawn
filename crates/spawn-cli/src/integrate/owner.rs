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
    /// nothing recorded; judged again after the backoff.
    NoVerdict(String),
    /// Nothing recorded, judged again at the next pass with no backoff:
    /// main moved (behind the integrator's back, or the commit is in main
    /// now, which step 0 records).
    Again(String),
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
            Err(Ok(_log)) => {
                self.land_owner(store, mirror, mission, request, &branch, &main, &candidate)
            }
            Err(Err(error)) => Judged::NoVerdict(error),
        }
    }

    /// The gate run of an owner judging: `Ok` with what it ended with when
    /// that is not an acceptance, `Err(Ok(log))` when the gate accepted
    /// `candidate` on `main`, `Err(Err(_))` when it could not be run.
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
    ) -> Result<Judged, Result<String, String>> {
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
        // A check main is red on is unsolved on the base for an owner's push
        // (the lead, 2026-11-10): the regression rule holds for what the base
        // solves, and a push during a red main must not have to go ungated.
        let mut gate_env = self.judging_env(&scenarios, jobs);
        gate_env.retain(|(name, _)| name != "SPAWN_GATE_RED_UNSOLVED");
        gate_env.push(("SPAWN_GATE_RED_UNSOLVED".to_string(), "1".to_string()));
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
        let refuse = |verdict: &str, reason: String, log: &str| {
            Ok(refuse_owner(
                store,
                request,
                reason,
                &json!({"verdict": verdict, "log": log}),
            ))
        };
        match judged {
            Ok(verdict) if verdict.accepted => Err(Ok(verdict.output)),
            Ok(verdict)
                if test_database
                    && let Some(fault) =
                        super::host::host_fault(&verdict.reason, &verdict.output, || {
                            self.crash_in(started, ended)
                        }) =>
            {
                Ok(Judged::NoVerdict(format!(
                    "the gate's rejection was the host's: {fault:?}"
                )))
            }
            Ok(verdict) => refuse(
                "rejected",
                format!("the owner's gate rejected it: {}", verdict.reason.trim()),
                &verdict.output,
            ),
            Err(failure) => Ok(Judged::NoVerdict(no_verdict(failure))),
        }
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
        if let Err(error) = moved_with(
            store,
            request,
            main,
            candidate,
            &json!({"verdict": "accepted"}),
        ) {
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
        GateFailure::Failed(error) => format!("the gate could not start: {error}"),
        GateFailure::NoRoom { short, .. } => format!("no room to judge ({})", short.check()),
        GateFailure::HostFault {
            scenario, reason, ..
        } => format!("the host's fault on {scenario}: {reason}"),
        GateFailure::CannotJudge { scenario, .. } => format!("the host cannot judge {scenario}"),
        GateFailure::BaseDisagrees { scenario, .. } => {
            format!("the base disagreed with itself on {scenario}")
        }
    }
}

/// An owner judging's end, on its thread: said, and its backoff kept
/// (`backoff`, shared with step 0): grown after no verdict (the host-fault
/// backoff from `base`), dropped otherwise.
pub(super) fn judged(
    said: &str,
    id: i64,
    judged: &Judged,
    backoff: &std::sync::Mutex<HashMap<i64, (u32, std::time::Instant)>>,
    base: std::time::Duration,
) {
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
        Judged::Landed { from, to } => {
            backoff.remove(&id);
            println!("{said}: accepted, main {from} -> {to}");
        }
    }
}
