//! The source's main merged into an agent's mounted, idle workspace, with
//! conflict markers when the two conflict (docs/storage.md, "Main into a
//! workspace").
//!
//! The Bridge merges main into a branch only when an execution starts or
//! resumes, and only when it merges cleanly ([`spawn_core::bridge`]). A
//! long-lived worker never restarts, so its branch would never get main, and
//! every piece of its work after main moved would be judged `conflict` with
//! nothing in the workspace to resolve. Two moments merge main into it
//! instead, both while the agent is idle at its prompt:
//!
//! - the integrator's `conflict` verdict ([`crate::integrate`]), before the
//!   verdict is recorded, so the note sent back names the files that now
//!   carry markers;
//! - the delivery of a task typed into a live worker
//!   ([`crate::dispatch::step`]), so the task starts from main and its
//!   brief says what the merge did.
//!
//! Both go through [`WorkspaceStore::merge_base_when`] of the mission's
//! store ([`sources::MissionWorkspaces`], which refreshes main from the
//! source): it takes the branch's lock, so it cannot race a checkpoint, and
//! it writes markers only where a content conflict shows them, leaving
//! `main` pending under `.merge/<agent>` as the second parent of the
//! checkpoint that resolves them. A clean merge is a `base_merged`
//! checkpoint, recorded, then pushed to the source before [`merge_main`]
//! returns (a push that fails stays queued on the process's pusher for the
//! source, which retries it); a conflict is recorded as `merge.conflicted`.
//!
//! **Only while the agent is idle.** Both moments are chosen while the
//! worker waits, but on facts read earlier: a human's input may have
//! started a turn since. So the store asks again, under the branch's lock,
//! after the fetch of main and right before it writes anything, on facts
//! read then ([`may_merge`], the rule a revision is typed in by). The Bridge
//! types under the same lock ([`WorkspaceStore::hold`]), so a turn typed in
//! meanwhile is seen. A worker that is not idle keeps its workspace as it
//! is: the merge is [`Merge::Busy`] and waits. The integrator's verdict then
//! says main is merged when the revision is typed in, and the dispatcher
//! merges it then; a delivery whose merge is busy is not sent this pass, and
//! is tried again on the next.
//!
//! **A re-assigned objective's work.** An ephemeral objective task whose
//! objective's earlier task ended without landing and left work starts from
//! that work, not from main ([`fork_from_prior`], called by the dispatcher
//! before the task's Start): its branch is created at the predecessor's last
//! checkpoint that is not a set-aside, and main is merged into it, cleanly
//! (a `base_merged` checkpoint, as at any start) or with its conflicts
//! marked as above, in the workspace mounted for the merge and committed
//! (a `base_merged` checkpoint naming no file) so the branch carries the
//! markers and the Start's mount keeps the pending merge. The Start's prompt
//! says which work it starts from and what the merge did ([`fork_note`]).
//!
//! A conflict markers cannot show (a file deleted on one side and changed on
//! the other, a rename, a binary file) changes nothing in the workspace but
//! a copy of main's version of each such file under [`MAIN_COPIES`], so the
//! worker, which cannot run git, has what it needs to resolve it. `.spawn/`
//! is never committed, and a mount exports the workspace afresh: the store
//! records the copies beside the workspace and every mount puts them back
//! while the conflict is open, so the note that names them still holds
//! after a resume.

use std::path::Path;
use std::sync::Arc;

use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, SessionId, WorkspaceId};
use spawn_core::port::{BaseMerge, Commit, EventLog, MERGE_WAITS, WorkspaceError, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::dispatch::Facts;
use crate::push::Pushed;
use crate::sources;

/// Where main's version of a file that conflicts without markers is copied,
/// relative to the workspace (`.spawn/` is never committed; every mount puts
/// the copies back while the conflict is open).
pub(crate) use spawn_host::MAIN_COPIES;

/// What merging main into an agent's workspace did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Merge {
    /// The branch already contains main (or a pending merge's markers are
    /// all resolved: the next checkpoint records it).
    Current,
    /// Main merged cleanly: the branch is at the merge `commit`; `files`
    /// changed in the workspace.
    Merged { commit: String, files: Vec<String> },
    /// Main conflicts with the branch, and `files` carry conflict markers in
    /// the workspace (written now, or by an earlier merge not resolved yet).
    Marked { files: Vec<String> },
    /// Main conflicts with the branch in `files` in a way markers cannot
    /// show; nothing in the workspace changed. `copied`: those of them main
    /// has, copied to [`MAIN_COPIES`]; the others main does not have.
    Unmarked {
        files: Vec<String>,
        copied: Vec<String>,
    },
    /// The agent has no workspace mounted under this root (or no mission
    /// with a source): nothing was merged.
    NotMounted,
    /// The agent was not idle (why): nothing was merged, and the merge
    /// waits for a moment it is ([`may_merge`]).
    Busy(String),
    /// The merge did not happen; why.
    Failed(String),
}

/// The workspace of `agent` mounted under `root`, if there is one.
fn mounted(root: &Path, agent: &str) -> Option<WorkspaceId> {
    let directory = root.join(agent);
    if !directory.is_dir() {
        return None;
    }
    WorkspaceId::new(format!("{agent}:{}", directory.display())).ok()
}

/// Merge main into the workspace of `agent` mounted under `root`, through
/// `workspaces` (whose `main` is the one in `mirror`). The git half of
/// [`merge_main`], without the database.
#[cfg(test)]
pub(crate) fn into_workspace(
    workspaces: &dyn WorkspaceStore,
    mirror: &Path,
    root: &Path,
    agent: &str,
) -> Merge {
    into_workspace_when(workspaces, mirror, root, agent, &|| Ok(()))
}

/// [`into_workspace`], only while `ready` says the agent is idle, asked by
/// the store right before it would write ([`WorkspaceStore::merge_base_when`]).
pub(crate) fn into_workspace_when(
    workspaces: &dyn WorkspaceStore,
    mirror: &Path,
    root: &Path,
    agent: &str,
    ready: &dyn Fn() -> Result<(), String>,
) -> Merge {
    let Some(workspace) = mounted(root, agent) else {
        return Merge::NotMounted;
    };
    match workspaces.merge_base_when(&workspace, ready) {
        Ok(BaseMerge::Current) => Merge::Current,
        Ok(BaseMerge::Merged { commit, files }) => Merge::Merged {
            commit: commit.as_str().to_string(),
            files,
        },
        Ok(BaseMerge::Conflicted {
            files,
            marked: true,
        }) => Merge::Marked { files },
        Ok(BaseMerge::Conflicted {
            files,
            marked: false,
        }) => {
            // The store copied main's versions and recorded which; without
            // the record no file is known to be one main does not have.
            match GitWorkspace::new(mirror, root).main_copies(agent) {
                Some((_, copied)) => Merge::Unmarked { files, copied },
                None => Merge::Failed(format!(
                    "the record of main's copies for {agent} cannot be read"
                )),
            }
        }
        Err(WorkspaceError::Refused { code, message }) if code == MERGE_WAITS => {
            Merge::Busy(message)
        }
        Err(error) => Merge::Failed(error.to_string()),
    }
}

/// Whether main may be merged into the workspace of the session `facts`
/// describe now, or why not: no command in flight in the session (none
/// being typed, no start, resume or stop under way), and a worker that runs
/// sits idle at its prompt, its turn stopped, judged on its latest fact
/// ([`Facts::turn_stopped_idle`], what a revision waits for before it is
/// typed in). A session with no execution running has no turn to disturb.
/// A session the facts do not list is not known to be idle.
pub(crate) fn may_merge(facts: Option<&Facts>) -> Result<(), String> {
    let Some(facts) = facts else {
        return Err("its session has no facts yet".to_string());
    };
    if facts.open_command {
        return Err("a command is in flight in its session".to_string());
    }
    if facts.stopping() {
        return Err("its worker's stop is under way".to_string());
    }
    if facts.live && !facts.turn_stopped_idle() {
        return Err("its worker is not idle at its prompt (a turn may be running)".to_string());
    }
    Ok(())
}

/// [`may_merge`] for `session`, on the dispatcher's facts read now, not the
/// ones a pass started with.
fn ready_to_merge(store: &PostgresStore, session: &str) -> Result<(), String> {
    let rows: Vec<Facts> = serde_json::from_value(
        store
            .dispatch_facts()
            .map_err(|error| format!("its session's facts could not be read: {error}"))?,
    )
    .map_err(|error| format!("its session's facts could not be read: {error}"))?;
    may_merge(rows.iter().find(|facts| facts.session_id == session))
}

/// Merge main into the mounted workspace of the mission agent `agent`
/// (session `session`) under `root`, through its mission's store, only while
/// the agent is idle ([`may_merge`], asked under the branch's lock), and
/// record what happened in the agent's session: a clean merge as a
/// `base_merged` `artifact.committed`, a conflict as `merge.conflicted`. A
/// merge commit is pushed before this returns; a push that failed is not
/// dropped but queued on the process's pusher for the source, which retries
/// it, and the integrator pushes a branch whose recorded checkpoint the
/// source lacks again ([`sources::push_recorded`]).
pub(crate) fn merge_main(
    store: &Arc<PostgresStore>,
    root: &Path,
    agent: &str,
    session: &str,
) -> Merge {
    let source = match store.agent_source(agent) {
        Ok(Some(Some(source))) => source,
        Ok(_) => return Merge::NotMounted,
        Err(error) => return Merge::Failed(format!("mission source of {agent}: {error}")),
    };
    if mounted(root, agent).is_none() {
        return Merge::NotMounted;
    }
    let mirror = match sources::ensure_mirror(root, Some(&source)) {
        Ok(mirror) => mirror,
        Err(error) => return Merge::Failed(error.to_string()),
    };
    let factory: sources::Factory = {
        let root = root.to_path_buf();
        Arc::new(move |repo: &Path| GitWorkspace::new(repo, &root))
    };
    let workspaces = sources::MissionWorkspaces::one_shot(
        GitWorkspace::new(&mirror, root),
        root,
        Arc::clone(store),
        factory,
    );
    let project = workspaces.project().to_string();
    // A merge whose record failed (here, or the Bridge's at a start) is
    // recorded from the branch now; one already recorded is a no-op.
    let earlier = head_merge(&mirror, agent);
    if let Some(earlier) = &earlier
        && let Err(error) = record(store, &project, agent, session, "-", earlier)
    {
        eprintln!("merge main into {agent}: record the earlier merge: {error}");
    }
    let ready = || ready_to_merge(store, session);
    let merge = into_workspace_when(&workspaces, &mirror, root, agent, &ready);
    let head = GitWorkspace::new(&mirror, root)
        .commit_id_of(&format!("refs/heads/{agent}"))
        .unwrap_or_else(|_| "-".to_string());
    // Recorded before the push: the published head has its record.
    if let Err(error) = record(store, &project, agent, session, &head, &merge) {
        eprintln!("merge main into {agent}: record (retried at its next merge): {error}");
    }
    if matches!(merge, Merge::Merged { .. }) || earlier.is_some() {
        publish_merge(&mirror, &source, agent, &head);
    }
    merge
}

/// The earlier task whose work an ephemeral objective task starts from
/// (`PostgresStore::objective_fork`): its id and its last checkpoint.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct PriorWork {
    pub(crate) task: i64,
    pub(crate) commit: String,
}

/// Create `agent`'s branch at `prior`'s checkpoint and merge main into it,
/// before its first execution starts (nothing of the agent runs). A clean
/// merge is a recorded `base_merged` checkpoint; a conflict is merged into
/// the workspace, mounted for it, with markers where they show, the markers
/// committed so the branch carries them (a `base_merged` checkpoint naming
/// no file, so no later delivery calls it a clean merge), and the
/// workspace unmounted; a conflict markers cannot show leaves main's copies
/// as at any merge. Everything is recorded in `session` and published. A
/// retried Start finds the branch there (`from` is then ignored) and merges
/// again, which finds main in, or the same conflict still marked.
pub(crate) fn fork_from_prior(
    store: &Arc<PostgresStore>,
    root: &Path,
    agent: &str,
    session: &str,
    prior: &PriorWork,
) -> Result<Fork, String> {
    let error = |what: &str, error: &dyn std::fmt::Display| {
        format!(
            "fork {agent} from task #{}'s work {}: {what}: {error}",
            prior.task, prior.commit
        )
    };
    let source = match store.agent_source(agent) {
        Ok(Some(Some(source))) => source,
        Ok(_) => return Ok(Fork::NoSource),
        Err(failure) => return Err(error("its mission source", &failure)),
    };
    let mirror = sources::ensure_mirror(root, Some(&source)).map_err(|e| error("mirror", &e))?;
    // The work is what the commit changed since its base with main: none
    // (a checkpoint main already holds) is no work to continue.
    sources::refresh(&mirror, &source).map_err(|e| error("refresh main", &e))?;
    let path = mirror.to_string_lossy();
    let base = sources::git(&[
        "--git-dir",
        &path,
        "merge-base",
        "refs/heads/main",
        &prior.commit,
    ])
    .map_err(|e| error("its base with main", &e))?;
    let changed = sources::git(&[
        "--git-dir",
        &path,
        "diff",
        "--name-only",
        &base,
        &prior.commit,
    ])
    .map_err(|e| error("its changes", &e))?;
    if changed.trim().is_empty() {
        return Ok(Fork::NoWork);
    }
    let factory: sources::Factory = {
        let root = root.to_path_buf();
        Arc::new(move |repo: &Path| GitWorkspace::new(repo, &root))
    };
    let workspaces = sources::MissionWorkspaces::one_shot(
        GitWorkspace::new(&mirror, root),
        root,
        Arc::clone(store),
        factory,
    );
    let project = workspaces.project().to_string();
    let branch = BranchName::new(agent).map_err(|e| error("branch", &e))?;
    workspaces
        .ensure_branch(&branch, Some(&prior.commit))
        .map_err(|e| error("create the branch", &e))?;
    let merge = match workspaces
        .merge_main_clean(&branch)
        .map_err(|e| error("merge main", &e))?
    {
        BaseMerge::Current => Merge::Current,
        BaseMerge::Merged { commit, files } => Merge::Merged {
            commit: commit.as_str().to_string(),
            files,
        },
        BaseMerge::Conflicted { .. } => {
            let merge =
                marked_in_workspace(store, &workspaces, &mirror, root, agent, session, prior)
                    .map_err(|(what, why)| error(what, &why))?;
            if let Merge::Failed(why) | Merge::Busy(why) = &merge {
                return Err(error("merge main into the workspace", why));
            }
            merge
        }
    };
    let head = GitWorkspace::new(&mirror, root)
        .commit_id_of(&format!("refs/heads/{agent}"))
        .unwrap_or_else(|_| "-".to_string());
    record(store, &project, agent, session, &head, &merge)
        .map_err(|e| error("record the merge", &e))?;
    if let Some(Pushed::Failed(why) | Pushed::Rejected(why)) =
        sources::publish(&mirror, &source, agent)
    {
        eprintln!("fork {agent}: the push of {head} failed; it stays queued and is retried: {why}");
    }
    Ok(Fork::Forked(merge))
}

/// Main merged into `agent`'s branch, which conflicts with it, in its
/// workspace mounted for the merge: markers where they show, committed so
/// the branch carries them (a `base_merged` checkpoint naming no file),
/// then unmounted. The merge, or what failed and why.
fn marked_in_workspace(
    store: &PostgresStore,
    workspaces: &sources::MissionWorkspaces,
    mirror: &Path,
    root: &Path,
    agent: &str,
    session: &str,
    prior: &PriorWork,
) -> Result<Merge, (&'static str, String)> {
    let branch = BranchName::new(agent).map_err(|e| ("branch", e.to_string()))?;
    let workspace = workspaces
        .mount(&branch)
        .map_err(|e| ("mount for the merge", e.to_string()))?;
    let merge = into_workspace_when(workspaces, mirror, root, agent, &|| Ok(()));
    let committed = if matches!(merge, Merge::Marked { .. }) {
        workspaces
            .commit(
                &workspace,
                &format!(
                    "spawn: main merged into the work of task #{}, its conflicts marked",
                    prior.task
                ),
            )
            .map_err(|e| ("commit the markers", e.to_string()))
    } else {
        Ok(Commit::Unchanged)
    };
    let unmounted = workspaces.unmount(&workspace);
    if let Commit::Created(commit) = committed? {
        let markers = Merge::Merged {
            commit: commit.as_str().to_string(),
            files: Vec::new(),
        };
        record(store, workspaces.project(), agent, session, "-", &markers)
            .map_err(|e| ("record the markers' checkpoint", e))?;
    }
    unmounted.map_err(|e| ("unmount", e.to_string()))?;
    Ok(merge)
}

/// What [`fork_from_prior`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fork {
    /// The branch starts from the work, with main merged in as `Merge`.
    Forked(Merge),
    /// The commit holds no change of its own since its base with main (a
    /// checkpoint main already holds): nothing forked, the task starts
    /// from main.
    NoWork,
    /// The agent has no mission source on this host: nothing forked.
    NoSource,
}

/// The head of `agent`'s branch on this host, if it has one: what a
/// Start finds after a fork that failed partway.
pub(crate) fn branch_head(store: &PostgresStore, root: &Path, agent: &str) -> Option<String> {
    let Ok(Some(Some(source))) = store.agent_source(agent) else {
        return None;
    };
    let mirror = sources::ensure_mirror(root, Some(&source)).ok()?;
    GitWorkspace::new(&mirror, root)
        .commit_id_of(&format!("refs/heads/{agent}"))
        .ok()
}

/// What a task is told when its fork from `prior`'s work kept failing
/// (`error`) and it starts without it: from the branch as far as the fork
/// got (`head`), or from main.
pub(crate) fn fork_failed_note(prior: &PriorWork, error: &str, head: Option<&str>) -> String {
    let opening = format!(
        "Spawn could not prepare your workspace from task #{}'s work (commit {}): {}.",
        prior.task,
        prior.commit,
        crate::deliver::one_line(error, 300)
    );
    match head {
        Some(head) => format!(
            "{opening} Your workspace holds that work as far as the preparation got (commit \
             {head}); the source's main is merged in at your start only where it merges \
             cleanly. Your brief's reports say what task #{} did.",
            prior.task
        ),
        None => format!(
            "{opening} Your workspace starts from the source's main; your brief's reports say \
             what task #{} did and found: use them, and do not redo what they established.",
            prior.task
        ),
    }
}

/// What a task forked from `prior`'s work is told at its start.
pub(crate) fn fork_note(prior: &PriorWork, merge: &Merge) -> String {
    let opening = format!(
        "Your workspace already contains task #{}'s work (its last checkpoint, commit {}), not \
         a fresh copy of main: continue it; do not redo it.",
        prior.task, prior.commit
    );
    let merged = match merge {
        Merge::Merged { files, .. } => format!(
            "The source's main was merged into it without a conflict; it changed {}. Re-read \
             those files before editing.",
            shown(files)
        ),
        Merge::Marked { files } => format!(
            "The source's main was merged into it, and these files carry conflict markers: {}. \
             {RESOLVE_MARKERS} Resolve them first.",
            files.join(", ")
        ),
        Merge::Unmarked { files, copied } => format!(
            "The source's main conflicts with it. {} Do that first.",
            unmarked_instructions(files, copied)
        ),
        Merge::Current | Merge::NotMounted | Merge::Busy(_) | Merge::Failed(_) => {
            "It already contains the source's main.".to_string()
        }
    };
    format!("{opening} {merged}")
}

/// Push the merge commit `head` of `agent`'s branch now, and say so when it
/// did not reach the source: a failed push stays queued and is retried
/// ([`sources::publish`]).
fn publish_merge(mirror: &Path, source: &str, agent: &str, head: &str) {
    match sources::publish(mirror, source, agent) {
        None | Some(Pushed::Done) => {}
        Some(Pushed::Failed(why)) => eprintln!(
            "merge main into {agent}: the push of {head} failed; it stays queued and is \
             retried: {why}"
        ),
        Some(Pushed::Rejected(why)) => {
            eprintln!("merge main into {agent}: the source refused {head}: {why}");
        }
    }
}

/// The head of `agent`'s branch in `mirror` as a clean merge of main, when it
/// is one (two parents, made by [`WorkspaceStore::merge_base`] or the
/// Bridge's start merge: "spawn: merge main …"): the commit and the files it
/// changed against the branch's own parent.
pub(crate) fn head_merge(mirror: &Path, agent: &str) -> Option<Merge> {
    let path = mirror.to_string_lossy();
    let shown = sources::git(&[
        "--git-dir",
        &path,
        "log",
        "-1",
        "--format=%H%n%P%n%s",
        &format!("refs/heads/{agent}"),
        "--",
    ])
    .ok()?;
    let mut lines = shown.lines();
    let (commit, parents, subject) = (lines.next()?, lines.next()?, lines.next()?);
    let parents: Vec<&str> = parents.split_whitespace().collect();
    if parents.len() != 2 || !subject.starts_with("spawn: merge main ") {
        return None;
    }
    let files = sources::git(&[
        "--git-dir",
        &path,
        "diff",
        "--name-only",
        "--no-renames",
        parents[0],
        commit,
    ])
    .ok()?;
    Some(Merge::Merged {
        commit: commit.to_string(),
        files: files.lines().map(str::to_string).collect(),
    })
}

/// The event that records `merge` in the agent's session, if it changed or
/// found anything. The keys make a repeat of the same merge or the same
/// conflict (the next delivery finding it pending) one record.
fn record(
    store: &PostgresStore,
    project: &str,
    agent: &str,
    session: &str,
    head: &str,
    merge: &Merge,
) -> Result<(), String> {
    let digest = |files: &[String]| sources::fnv(&files.join("\0"));
    let (key, kind) = match merge {
        Merge::Merged { commit, files } => (
            format!("commit:{commit}"),
            EventKind::ArtifactCommitted {
                reason: CheckpointReason::BaseMerged,
                artifact: ArtifactRef {
                    project: project.to_string(),
                    branch: BranchName::new(agent).map_err(|error| error.to_string())?,
                    commit: CommitId::new(commit.clone()).map_err(|error| error.to_string())?,
                },
                files: files.clone(),
            },
        ),
        Merge::Marked { files } => (
            format!("conflict-marked:{head}:{:016x}", digest(files)),
            EventKind::MergeConflicted {
                files: files.clone(),
                marked: true,
            },
        ),
        Merge::Unmarked { files, .. } => (
            format!("conflict:{head}:{:016x}", digest(files)),
            EventKind::MergeConflicted {
                files: files.clone(),
                marked: false,
            },
        ),
        Merge::Current | Merge::NotMounted | Merge::Busy(_) | Merge::Failed(_) => return Ok(()),
    };
    store
        .append(NewEvent {
            agent_id: AgentId::new(agent).map_err(|error| error.to_string())?,
            session_id: SessionId::new(session).map_err(|error| error.to_string())?,
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(key),
            kind,
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// How to resolve a conflict markers cannot show, for a worker that cannot
/// run git.
fn unmarked_instructions(files: &[String], copied: &[String]) -> String {
    let missing: Vec<&str> = files
        .iter()
        .filter(|file| !copied.contains(file))
        .map(String::as_str)
        .collect();
    let mut steps = Vec::new();
    if !copied.is_empty() {
        steps.push(format!(
            "make each of {} exactly main's version, which is copied to `{MAIN_COPIES}/<the same \
             path>` (copy it over yours)",
            copied.join(", ")
        ));
    }
    if !missing.is_empty() {
        steps.push(format!(
            "delete {} from your workspace: main does not have {}",
            missing.join(", "),
            if missing.len() == 1 { "it" } else { "them" }
        ));
    }
    format!(
        "These files conflict in a way conflict markers cannot show (a file deleted on one side \
         and changed on the other, a rename, or a binary file): {}. Nothing in your workspace was \
         changed. To resolve it: {}. Your own change to such a file is then lost: redo it on top \
         of main's version for a text file, or put it in a new file.",
        files.join(", "),
        steps.join("; then ")
    )
}

/// How to resolve markers.
const RESOLVE_MARKERS: &str = "Resolve them by editing the marked regions (<<<<<<< / ======= / \
     >>>>>>>), keeping both sides' intent and removing every marker; do not rewrite the files \
     from memory. The checkpoint after the last marker is gone records the merge, with main as \
     its second parent.";

impl Merge {
    /// Its name in a `conflict` verdict's record (`merge`): whether the
    /// worker got conflicts to resolve (`marked`, `unmarked`) or main could
    /// not go in, or went in cleanly (the others). The objective queue counts
    /// only the first kind as a failed attempt (`objectives::Outcome`).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Merge::Current => "current",
            Merge::Merged { .. } => "merged",
            Merge::Marked { .. } => "marked",
            Merge::Unmarked { .. } => "unmarked",
            Merge::NotMounted => "not_mounted",
            Merge::Busy(_) => "busy",
            Merge::Failed(_) => "failed",
        }
    }
}

/// The note a `conflict` verdict sends back, after `merge` merged `main`
/// into the workspace.
pub(crate) fn conflict_note(main: &str, merge: &Merge) -> String {
    let opening = format!("Your work conflicts with the source's current main ({main}).");
    match merge {
        Merge::Marked { files } => format!(
            "{opening} Main is now merged into your workspace, and these files carry conflict \
             markers: {}. {RESOLVE_MARKERS} Then report again.",
            files.join(", ")
        ),
        Merge::Unmarked { files, copied } => format!(
            "{opening} {} Then report again.",
            unmarked_instructions(files, copied)
        ),
        Merge::Merged { files, .. } => format!(
            "{opening} Main has now been merged into your workspace without a conflict (it \
             changed {}). Check that your work still holds with those files, then report again.",
            shown(files)
        ),
        Merge::Current => format!(
            "{opening} Your workspace already holds main (any markers there are resolved). \
             Report again: your next checkpoint records the merge."
        ),
        Merge::NotMounted => format!(
            "{opening} Your workspace is not mounted on the integrator's host, so main could not \
             be merged into it. Once it is mounted again (your worker resumed), main is merged \
             into it if it merges cleanly; if not, your next report is sent back again with main \
             merged in and the conflicting regions marked, for you to resolve."
        ),
        Merge::Busy(why) => format!(
            "{opening} Main could not be merged into your workspace when your work was judged \
             ({why}): nothing in it was changed. Main is merged into it right before this note is \
             typed in, and what that merge did is said after this note. If your worker is \
             resumed instead, main is merged into it if it merges cleanly; if not, your next \
             report is sent back again with main merged in and the conflicting regions marked, \
             for you to resolve."
        ),
        Merge::Failed(error) => format!(
            "{opening} Merging main into your workspace failed ({error}). Report again: the next \
             verdict tries the merge again."
        ),
    }
}

/// What a task's brief says about `merge`, done right before the task was
/// typed in, and about the clean merges recorded since the worker was last
/// typed a delivery (`untold`: the files they changed,
/// [`PostgresStore::untold_merges`]). A delivery refused before typing
/// (a pane not ready) is retried, and the retry's merge finds main already
/// in: what the first one merged is told from its record. Nothing when no
/// merge is untold.
pub(crate) fn delivery_note(merge: &Merge, untold: &[String]) -> Option<String> {
    let opening = "Before this task, the source's main was merged into your workspace";
    let clean = |files: &[String]| {
        let mut files: Vec<String> = untold.iter().chain(files).cloned().collect();
        files.sort();
        files.dedup();
        format!(
            "{opening}, without a conflict; it changed {}. Re-read those files before editing.",
            shown(&files)
        )
    };
    match merge {
        Merge::Merged { files, .. } => Some(clean(files)),
        Merge::Marked { files } => Some(format!(
            "{opening}, and these files carry conflict markers: {}. {RESOLVE_MARKERS} Resolve \
             them first.",
            files.join(", ")
        )),
        Merge::Unmarked { files, copied } => Some(format!(
            "{opening}, but it conflicts with your branch. {} Do that first.",
            unmarked_instructions(files, copied)
        )),
        Merge::Current | Merge::NotMounted | Merge::Busy(_) | Merge::Failed(_) => {
            (!untold.is_empty()).then(|| clean(&[]))
        }
    }
}

fn shown(files: &[String]) -> String {
    if files.is_empty() {
        "no file".to_string()
    } else {
        files.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{Merge, conflict_note, delivery_note, into_workspace, into_workspace_when};
    use crate::push::{Pushed, Pusher, push_branch};
    use crate::sources::{ensure_mirror, fetch_pushed_into, test_git as git};
    use spawn_core::id::{BranchName, WorkspaceId};
    use spawn_core::port::{Commit, WorkspaceStore};
    use spawn_host::GitWorkspace;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Audit #22: a conflict judged while the workspace is not mounted is
    /// not left to a person: once mounted, the next report's `conflict`
    /// verdict merges main with markers, and the worker resolves them.
    #[test]
    fn a_conflict_while_not_mounted_comes_back_marked_for_the_worker() {
        let note = conflict_note("abc123", &Merge::NotMounted);
        assert!(!note.contains("a person"), "{note}");
        assert!(note.contains("marked, for you to resolve"), "{note}");
    }

    fn commit_in(directory: &Path, message: &str) {
        let directory = directory.to_string_lossy().to_string();
        git(&["-C", &directory, "add", "-A"]).expect("add");
        git(&[
            "-C",
            &directory,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--no-gpg-sign",
            "-qm",
            message,
        ])
        .expect("commit");
    }

    /// A bare source with README.md and notes.txt on main, this host's
    /// mirror of it under the workspace root, and the store over the
    /// mirror: `(base, source, root, mirror, store)`.
    fn world(tag: &str) -> (PathBuf, String, PathBuf, PathBuf, GitWorkspace) {
        let base = std::env::temp_dir().join(format!("spawn-merge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        git(&[
            "init",
            "-q",
            "--initial-branch=main",
            &seed.to_string_lossy(),
        ])
        .expect("init");
        std::fs::write(seed.join("README.md"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(seed.join("notes.txt"), "notes\n").expect("write");
        commit_in(&seed, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed.to_string_lossy(), &source]).expect("source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let store = GitWorkspace::new(&mirror, &root);
        (base, source, root, mirror, store)
    }

    /// Main moves on the source by `change`, and the mirror follows; main's
    /// new commit.
    fn main_moves(base: &Path, source: &str, mirror: &Path, change: impl Fn(&Path)) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let clone = base.join(format!("clone-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        change(&clone);
        commit_in(&clone, "main moves");
        git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
        crate::sources::refresh(mirror, source).expect("refresh");
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "refs/heads/main",
        ])
        .expect("main")
    }

    /// `agent`'s branch with one checkpoint by `work`, mounted.
    fn agent_with_work(store: &GitWorkspace, agent: &str, work: impl Fn(&Path)) -> WorkspaceId {
        let branch = BranchName::new(agent).expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        work(Path::new(workspace.path()));
        assert!(matches!(
            store.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        workspace
    }

    fn head(mirror: &Path, agent: &str) -> String {
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            &format!("refs/heads/{agent}"),
        ])
        .expect("head")
    }

    /// Facts of a live worker idle at its prompt: its turn stopped, judged
    /// on its latest fact, nothing in flight.
    fn idle_worker() -> crate::dispatch::Facts {
        use spawn_core::event::SessionSeq;
        use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};
        let mut facts: crate::dispatch::Facts = serde_json::from_value(serde_json::json!({
            "session_id": "session_worker_a",
            "agent_id": "agent_worker_a",
            "live": true,
            "execution_id": "exec_5",
            "evidence_seq": 9,
            "turn_ended": true,
        }))
        .expect("facts");
        let mut observation = Observation::basic(
            SemanticState::Waiting,
            None,
            SessionSeq(9),
            Confidence::new(0.9).expect("valid"),
            "jev",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        facts.observation = Some(observation);
        facts
    }

    /// Issue #7: main is merged into a workspace only while its agent is
    /// idle, by the rule a revision is typed in: nothing in flight in the
    /// session, no stop under way, and a live worker stopped at its prompt
    /// on its latest fact. A session with no execution has no turn.
    #[test]
    fn main_is_merged_only_into_an_idle_worker() {
        use super::may_merge;
        let idle = idle_worker();
        assert_eq!(may_merge(Some(&idle)), Ok(()));

        let mut typing = idle_worker();
        typing.open_command = true;
        assert!(may_merge(Some(&typing)).is_err(), "an input in flight");

        let mut working = idle_worker();
        working.evidence_seq = Some(12);
        assert!(
            may_merge(Some(&working)).is_err(),
            "a fact newer than the idle judgement: a turn may be running"
        );

        let mut running = idle_worker();
        if let Some(observation) = running.observation.as_mut() {
            observation.ambiguity = None;
            observation.state = spawn_core::observation::SemanticState::Working;
        }
        assert!(may_merge(Some(&running)).is_err(), "judged working");

        let mut stopping = idle_worker();
        stopping.stops = vec![crate::dispatch::StopSent {
            id: 3,
            key: "retire-x".to_string(),
            principal: "spawn-dispatch".to_string(),
            action: "stop".to_string(),
            created_at_ms: 1,
            finished_at_ms: Some(2),
            succeeded: Some(false),
            error: Some("pane gone".to_string()),
        }];
        assert!(may_merge(Some(&stopping)).is_err(), "a stop under way");

        let mut ended = idle_worker();
        ended.live = false;
        ended.observation = None;
        assert_eq!(may_merge(Some(&ended)), Ok(()), "no execution runs");
        ended.open_command = true;
        assert!(may_merge(Some(&ended)).is_err(), "a resume in flight");

        assert!(may_merge(None).is_err(), "no facts: not known idle");
    }

    /// Issue #7: a merge whose agent is found busy right before it would
    /// write changes nothing (no merge commit, no file of main's), and the
    /// conflict note says main comes with the revision; once the agent is
    /// idle, the same call merges.
    #[test]
    fn a_merge_found_busy_changes_nothing_and_says_when_main_comes() {
        let (base, source, root, mirror, store) = world("busy");
        let agent = "agent_busy";
        agent_with_work(&store, agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        let before = head(&mirror, agent);
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });
        let merge = into_workspace_when(&store, &mirror, &root, agent, &|| {
            Err("its worker is not idle at its prompt (a turn may be running)".to_string())
        });
        let Merge::Busy(why) = &merge else {
            panic!("the merge waits: {merge:?}");
        };
        assert!(why.contains("not idle"), "{why}");
        assert_eq!(head(&mirror, agent), before, "no merge commit");
        assert!(!root.join(agent).join("added.txt").exists());
        let note = conflict_note(&main, &merge);
        assert!(
            note.contains("nothing in it was changed")
                && note.contains("right before this note is typed in")
                && note.contains("not idle"),
            "{note}"
        );
        assert_eq!(delivery_note(&merge, &[]), None);

        // Idle now: the merge happens.
        assert!(matches!(
            into_workspace_when(&store, &mirror, &root, agent, &|| Ok(())),
            Merge::Merged { .. }
        ));
        assert!(root.join(agent).join("added.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #8: the push of a merge commit fails once (a ref lock lost to
    /// another writer, a source briefly away); the commit still reaches the
    /// source, where the integrator fetches it: the failed push stayed
    /// queued and was retried, where the one-shot flush used to drop it and
    /// leave the task `stale` forever.
    #[test]
    fn a_merge_whose_push_fails_once_reaches_the_source_on_the_retry() {
        let (base, source, root, mirror, store) = world("push-fails");
        let agent = "agent_push";
        agent_with_work(&store, agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });
        let Merge::Merged { commit, .. } = into_workspace(&store, &mirror, &root, agent) else {
            panic!("a clean merge");
        };
        let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let once = std::sync::Arc::clone(&failed);
        let pusher = Pusher::with(
            Box::new(move |mirror: &Path, source: &str, branch: &str| {
                if once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    push_branch(mirror, source, branch)
                } else {
                    Pushed::Failed("cannot lock ref 'refs/heads/spawn/agent_push'".to_string())
                }
            }),
            Duration::from_millis(1),
            Duration::from_millis(20),
        );
        assert!(matches!(
            crate::sources::publish_with(&pusher, &mirror, &source, agent),
            Some(Pushed::Failed(_))
        ));
        let landing = format!("refs/spawn-integrate/{agent}");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut published = None;
        while Instant::now() < deadline {
            published = fetch_pushed_into(&mirror, &source, agent, &landing).expect("fetch");
            if published.as_deref() == Some(commit.as_str()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(published, Some(commit), "the retry published the merge");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #8: a recorded checkpoint the source lacks (its push lost with
    /// the process that queued it) is pushed again by the integrator's stale
    /// check; a head past the record is not.
    #[test]
    fn a_recorded_checkpoint_the_source_lacks_is_pushed_again() {
        let (base, source, _root, mirror, store) = world("push-again");
        let agent = "agent_again";
        agent_with_work(&store, agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        let recorded = head(&mirror, agent);
        let landing = format!("refs/spawn-integrate/{agent}");
        assert_eq!(
            fetch_pushed_into(&mirror, &source, agent, &landing).expect("fetch"),
            None
        );
        assert_eq!(
            crate::sources::push_recorded(&mirror, &source, agent, "0123abcd"),
            None,
            "a head that is not the record is left to its own push"
        );
        assert_eq!(
            crate::sources::push_recorded(&mirror, &source, agent, &recorded),
            Some(Pushed::Done)
        );
        assert_eq!(
            fetch_pushed_into(&mirror, &source, agent, &landing).expect("fetch"),
            Some(recorded)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #11, at the integrator's verdict: the note for a conflict
    /// markers cannot show promises main's copies, and they are there again
    /// after the workspace was unmounted and a resume mounted it afresh
    /// (the Bridge's order: a clean-only merge, then the mount).
    #[test]
    fn the_unmarked_note_holds_after_a_resume_remounts_the_workspace() {
        let (base, source, root, mirror, store) = world("unmarked-resume");
        let agent = "agent_unmarked";
        let workspace = agent_with_work(&store, agent, |directory| {
            std::fs::remove_file(directory.join("notes.txt")).expect("delete");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("notes.txt"), "notes, revised\n").expect("write");
        });
        let merge = into_workspace(&store, &mirror, &root, agent);
        assert_eq!(
            merge,
            Merge::Unmarked {
                files: vec!["notes.txt".to_string()],
                copied: vec!["notes.txt".to_string()],
            }
        );
        let note = conflict_note(&main, &merge);
        assert!(note.contains(".spawn/main"), "{note}");
        store.unmount(&workspace).expect("the engine is lost");
        let branch = BranchName::new(agent).expect("valid branch");
        store.merge_main_clean(&branch).expect("the resume's merge");
        store.mount(&branch).expect("the resume's mount");
        assert_eq!(
            std::fs::read_to_string(root.join(agent).join(super::MAIN_COPIES).join("notes.txt"))
                .expect("the copy the note names"),
            "notes, revised\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A schema of its own in the test database, dropped afterwards.
    struct Db {
        store: std::sync::Arc<spawn_postgres::PostgresStore>,
        url: String,
        schema: String,
    }

    impl Db {
        fn new(case: &str) -> Self {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
            let store =
                spawn_postgres::PostgresStore::connect_in_schema(&url, &schema).expect("connect");
            Self {
                store: std::sync::Arc::new(store),
                url,
                schema,
            }
        }

        /// A task on a mission whose source is `source`: its agent and
        /// session, with one event, so the session has facts.
        fn task_on(&self, source: &str, label: &str) -> (String, String) {
            use spawn_core::event::{EventKind, NewEvent, Source};
            use spawn_core::id::{AgentId, SessionId};
            use spawn_core::port::EventLog;
            let mission = self
                .store
                .mission_add(label, &serde_json::json!([source]), "human")
                .expect("mission");
            let task = self
                .store
                .task_assign(mission, "ephemeral", label, "brief", "default")
                .expect("task");
            let mut client =
                postgres::Client::connect(&self.url, postgres::NoTls).expect("connection");
            client
                .batch_execute(&format!("set search_path to {}", self.schema))
                .expect("schema");
            let row = client
                .query_one(
                    "select agent_id, session_id from mission_tasks where id = $1",
                    &[&task],
                )
                .expect("the task");
            let (agent, session): (String, String) = (row.get(0), row.get(1));
            self.store
                .append(NewEvent {
                    agent_id: AgentId::new(agent.clone()).expect("agent"),
                    session_id: SessionId::new(session.clone()).expect("session"),
                    execution_id: None,
                    source: Source::Observer,
                    source_event_id: None,
                    kind: EventKind::MessageReceived,
                })
                .expect("an event");
            (agent, session)
        }
    }

    /// An objective mission on `source` whose resident holds objective 9,
    /// and two tasks of objective 7: the first, ephemeral, did `work` on its
    /// branch (one recorded checkpoint) and yielded its slot; the second,
    /// ephemeral too, is assigned now. `(first, second)` as
    /// `(task, agent, session)`, and the mission.
    #[allow(clippy::type_complexity)]
    fn reassigned(
        db: &Db,
        store: &GitWorkspace,
        mirror: &Path,
        source: &str,
        work: impl Fn(&Path),
    ) -> ((i64, String, String), (i64, String, String), i64) {
        use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
        use spawn_core::id::{AgentId, CommitId, SessionId};
        use spawn_core::port::EventLog;
        db.store
            .resident_propose("rf", "objectives")
            .expect("propose");
        db.store.resident_approve("rf").expect("approve");
        let mission = db
            .store
            .mission_add("fork", &serde_json::json!([source]), "human")
            .expect("mission");
        db.store
            .mission_configure(
                mission,
                Some("/nowhere/scenarios"),
                "gate",
                None,
                Some(&serde_json::json!({"backlog": "/nowhere", "resident": "rf",
                                          "workers": 3})),
            )
            .expect("configure");
        let task = |objective: i32| {
            let id = db
                .store
                .objective_assign(mission, objective, None, "brief")
                .expect("assign");
            let mut client =
                postgres::Client::connect(&db.url, postgres::NoTls).expect("connection");
            client
                .batch_execute(&format!("set search_path to {}", db.schema))
                .expect("schema");
            let row = client
                .query_one(
                    "select agent_id, session_id from mission_tasks where id = $1",
                    &[&id],
                )
                .expect("the task");
            (id, row.get::<_, String>(0), row.get::<_, String>(1))
        };
        let _resident = task(9);
        let first = task(7);
        let workspace = agent_with_work(store, &first.1, work);
        store.unmount(&workspace).expect("its execution ended");
        db.store
            .append(NewEvent {
                agent_id: AgentId::new(first.1.clone()).expect("agent"),
                session_id: SessionId::new(first.2.clone()).expect("session"),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: None,
                kind: EventKind::ArtifactCommitted {
                    reason: CheckpointReason::ToolCompleted,
                    artifact: ArtifactRef {
                        project: "missions".to_string(),
                        branch: BranchName::new(first.1.clone()).expect("branch"),
                        commit: CommitId::new(head(mirror, &first.1)).expect("commit"),
                    },
                    files: vec!["work".to_string()],
                },
            })
            .expect("its checkpoint");
        db.store
            .integrator_record(&serde_json::json!({"type": "task.cancelled", "id": first.0,
                                                    "yielded": true}))
            .expect("it yields");
        let second = task(7);
        (first, second, mission)
    }

    /// The Start the dispatcher sends for `task`'s first delivery.
    fn start(task: &(i64, String, String)) -> crate::dispatch::Order {
        crate::dispatch::Order {
            agent_id: task.1.clone(),
            session_id: task.2.clone(),
            key: format!("deliver-{}-0", task.0),
            action: spawn_core::command::CommandAction::Start {
                engine: "claude".to_string(),
                prompt: "the task".to_string(),
                parent_session_id: None,
                from: None,
                model: None,
                charter: None,
            },
            task: Some(task.0),
        }
    }

    fn prompt(order: &crate::dispatch::Order) -> &str {
        let spawn_core::command::CommandAction::Start { prompt, .. } = &order.action else {
            panic!("a start");
        };
        prompt
    }

    fn show(mirror: &Path, commit: &str, file: &str) -> Option<String> {
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "show",
            &format!("{commit}:{file}"),
        ])
        .ok()
    }

    /// The prior-work fork: a re-assigned objective's ephemeral task starts
    /// from the work its yielded predecessor left, with main merged in
    /// cleanly, recorded and said in the Start's prompt; a task of an
    /// objective with no earlier work forks from main at its Start, as
    /// before (no branch made, the prompt unchanged).
    #[test]
    fn a_reassigned_task_starts_from_the_work_its_predecessor_left() {
        let db = Db::new("prior_fork");
        let (base, source, root, mirror, store) = world("prior-fork");
        let (first, second, mission) = reassigned(&db, &store, &mirror, &source, |directory| {
            std::fs::write(directory.join("work.txt"), "half of it\n").expect("write");
        });
        let checkpoint = head(&mirror, &first.1);
        main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });
        let mut order = start(&second);
        let line = crate::dispatch::fork_before(&db.store, &root, &mut order)
            .expect("the fork")
            .expect("it forked");
        assert!(
            line.contains(&format!("task #{}'s work {checkpoint}", first.0)),
            "{line}"
        );
        let forked = head(&mirror, &second.1);
        assert_eq!(
            show(&mirror, &forked, "work.txt").as_deref(),
            Some("half of it")
        );
        assert_eq!(
            show(&mirror, &forked, "added.txt").as_deref(),
            Some("from main")
        );
        assert_eq!(
            db.store
                .latest_checkpoint(&second.1)
                .expect("checkpoint")
                .as_deref(),
            Some(forked.as_str()),
            "the merge is recorded"
        );
        let told = prompt(&order);
        assert!(told.starts_with("the task\n\n"), "{told}");
        assert!(
            told.contains(&format!(
                "Your workspace already contains task #{}'s work (its last checkpoint, commit \
                 {checkpoint}), not a fresh copy of main: continue it",
                first.0
            )) && told.contains("without a conflict; it changed added.txt"),
            "{told}"
        );

        // Another objective, never tried: nothing.
        let fresh = db
            .store
            .objective_assign(mission, 8, None, "brief")
            .expect("assign");
        let (agent, session) = {
            let mut client =
                postgres::Client::connect(&db.url, postgres::NoTls).expect("connection");
            client
                .batch_execute(&format!("set search_path to {}", db.schema))
                .expect("schema");
            let row = client
                .query_one(
                    "select agent_id, session_id from mission_tasks where id = $1",
                    &[&fresh],
                )
                .expect("the task");
            (row.get::<_, String>(0), row.get::<_, String>(1))
        };
        let mut order = start(&(fresh, agent.clone(), session));
        assert_eq!(
            crate::dispatch::fork_before(&db.store, &root, &mut order).expect("no fork"),
            None
        );
        assert_eq!(prompt(&order), "the task");
        assert!(
            git(&[
                "--git-dir",
                &mirror.to_string_lossy(),
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{agent}"),
            ])
            .is_err(),
            "its branch forks from main at the Start"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The prior-work fork when main conflicts with the work: the markers
    /// are written and committed, the Start's prompt names the files, the
    /// Start's own merge and mount keep them, and the checkpoint that
    /// resolves them records the merge with main as its second parent.
    #[test]
    fn a_reassigned_task_gets_main_with_its_conflicts_marked() {
        let db = Db::new("prior_fork_conflict");
        let (base, source, root, mirror, store) = world("prior-fork-conflict");
        let (first, second, _) = reassigned(&db, &store, &mirror, &source, |directory| {
            std::fs::write(directory.join("README.md"), "one\ntwo (work)\nthree\n").expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("README.md"), "one\ntwo (main)\nthree\n").expect("write");
        });
        let mut order = start(&second);
        crate::dispatch::fork_before(&db.store, &root, &mut order)
            .expect("the fork")
            .expect("it forked");
        let told = prompt(&order);
        assert!(
            told.contains(&format!("already contains task #{}'s work", first.0))
                && told.contains("these files carry conflict markers: README.md")
                && told.contains("Resolve them first"),
            "{told}"
        );
        let forked = head(&mirror, &second.1);
        let readme = show(&mirror, &forked, "README.md").expect("README.md");
        assert!(
            readme.contains("<<<<<<<") && readme.contains("two (work)"),
            "{readme}"
        );
        assert!(store.has_pending_merge(&second.1));
        assert!(!root.join(&second.1).exists(), "unmounted until its Start");
        assert_eq!(
            db.store
                .latest_checkpoint(&second.1)
                .expect("checkpoint")
                .as_deref(),
            Some(forked.as_str()),
            "the markers' checkpoint is recorded"
        );

        // The Bridge's Start: its clean merge finds the conflict, its mount
        // keeps the markers and the pending merge.
        let branch = BranchName::new(second.1.clone()).expect("branch");
        assert!(matches!(
            store.merge_main_clean(&branch).expect("the start's merge"),
            spawn_core::port::BaseMerge::Conflicted { .. }
        ));
        let workspace = store.mount(&branch).expect("the start's mount");
        let directory = Path::new(workspace.path());
        let mounted = std::fs::read_to_string(directory.join("README.md")).expect("read");
        assert!(mounted.contains("<<<<<<<"), "{mounted}");
        assert!(store.has_pending_merge(&second.1));
        // The worker resolves them.
        std::fs::write(directory.join("README.md"), "one\ntwo (both)\nthree\n").expect("write");
        assert!(matches!(
            store.commit(&workspace, "resolved").expect("commit"),
            Commit::Created(_)
        ));
        let parents = git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-list",
            "--parents",
            "-n",
            "1",
            &format!("refs/heads/{}", second.1),
        ])
        .expect("parents");
        assert!(
            parents.ends_with(&main),
            "main is the second parent: {parents}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A checkpoint `commit` recorded in `task`'s session, with `reason`
    /// and, for a set-aside, its key.
    fn checkpoint_record(
        db: &Db,
        task: &(i64, String, String),
        commit: &str,
        reason: spawn_core::event::CheckpointReason,
        key: Option<String>,
    ) {
        use spawn_core::event::{ArtifactRef, EventKind, NewEvent, Source};
        use spawn_core::id::{AgentId, CommitId, SessionId};
        use spawn_core::port::EventLog;
        db.store
            .append(NewEvent {
                agent_id: AgentId::new(task.1.clone()).expect("agent"),
                session_id: SessionId::new(task.2.clone()).expect("session"),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: key,
                kind: EventKind::ArtifactCommitted {
                    reason,
                    artifact: ArtifactRef {
                        project: "missions".to_string(),
                        branch: BranchName::new(task.1.clone()).expect("branch"),
                        commit: CommitId::new(commit.to_string()).expect("commit"),
                    },
                    files: Vec::new(),
                },
            })
            .expect("a checkpoint");
    }

    fn assigned(db: &Db, mission: i64, objective: i32) -> (i64, String, String) {
        let id = db
            .store
            .objective_assign(mission, objective, None, "brief")
            .expect("assign");
        let mut client = postgres::Client::connect(&db.url, postgres::NoTls).expect("connection");
        client
            .batch_execute(&format!("set search_path to {}", db.schema))
            .expect("schema");
        let row = client
            .query_one(
                "select agent_id, session_id from mission_tasks where id = $1",
                &[&id],
            )
            .expect("the task");
        (id, row.get(0), row.get(1))
    }

    /// rev-e on a114a9e: a resident predecessor. Its window ends at its
    /// task's set-aside, so the checkpoint picked is its work, not the
    /// set-aside nor the merge of main recorded on top of it for its next
    /// delivery (whose tree is main: objective 55's task 149). A resident
    /// checkpoint main already holds is no work: the task starts from main.
    #[test]
    fn a_resident_predecessors_work_is_its_checkpoint_before_its_set_aside() {
        use spawn_core::event::CheckpointReason;
        let db = Db::new("prior_fork_resident");
        let (base, source, root, mirror, store) = world("prior-fork-resident");
        db.store
            .resident_propose("rr", "objectives")
            .expect("propose");
        db.store.resident_approve("rr").expect("approve");
        let mission = db
            .store
            .mission_add("fork", &serde_json::json!([source]), "human")
            .expect("mission");
        db.store
            .mission_configure(
                mission,
                Some("/nowhere/scenarios"),
                "gate",
                None,
                Some(&serde_json::json!({"backlog": "/nowhere", "resident": "rr",
                                          "workers": 3})),
            )
            .expect("configure");
        // The resident's task for objective 7 works, then yields; its work
        // is set aside and main merged on top for its next task.
        let first = assigned(&db, mission, 7);
        let workspace = agent_with_work(&store, &first.1, |directory| {
            std::fs::write(directory.join("work.txt"), "the resident's\n").expect("write");
        });
        store.unmount(&workspace).expect("unmount");
        let work = head(&mirror, &first.1);
        checkpoint_record(&db, &first, &work, CheckpointReason::ToolCompleted, None);
        db.store
            .integrator_record(&serde_json::json!({"type": "task.cancelled", "id": first.0,
                                                    "yielded": true}))
            .expect("it yields");
        let main = head(&mirror, "main");
        checkpoint_record(
            &db,
            &first,
            &"5".repeat(40),
            CheckpointReason::SetAside,
            Some(format!("set-aside:{}", first.0)),
        );
        checkpoint_record(&db, &first, &main, CheckpointReason::BaseMerged, None);
        let _resident = assigned(&db, mission, 9);
        let second = assigned(&db, mission, 7);
        assert_ne!(second.1, first.1, "an ephemeral worker");
        assert_eq!(
            db.store.objective_fork(second.0).expect("fork"),
            serde_json::json!({"task": first.0, "commit": work})
        );
        let mut order = start(&second);
        crate::dispatch::fork_before(&db.store, &root, &mut order)
            .expect("the fork")
            .expect("it forked");
        assert_eq!(
            show(&mirror, &head(&mirror, &second.1), "work.txt").as_deref(),
            Some("the resident's")
        );

        // Objective 8's earlier task recorded a checkpoint main already
        // holds: no work, so its next task starts from main.
        let third = assigned(&db, mission, 8);
        checkpoint_record(&db, &third, &main, CheckpointReason::ToolCompleted, None);
        db.store
            .integrator_record(&serde_json::json!({"type": "task.cancelled", "id": third.0,
                                                    "yielded": true}))
            .expect("it yields");
        let fourth = assigned(&db, mission, 8);
        assert_eq!(
            db.store.objective_fork(fourth.0).expect("fork")["task"],
            third.0,
            "the query picks it; git finds no work in it"
        );
        let mut order = start(&fourth);
        let line = crate::dispatch::fork_before(&db.store, &root, &mut order)
            .expect("no fork")
            .expect("said");
        assert!(line.contains("holds nothing main lacks"), "{line}");
        assert_eq!(
            prompt(&order),
            format!(
                "the task\n\nTask #{}'s last checkpoint (commit {main}) holds nothing the \
                 source's main lacks: its work is already in main, and your workspace starts \
                 from main.",
                third.0
            ),
            "the brief's mention of that checkpoint does not send it looking"
        );
        assert!(
            git(&[
                "--git-dir",
                &mirror.to_string_lossy(),
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", fourth.1),
            ])
            .is_err(),
            "its branch forks from main at the Start"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// GitHub #16 item 1: the set-aside moves the branch before it is
    /// recorded. An append that fails (here the log refuses the session)
    /// leaves the set-aside commit without its `artifact.committed`; the
    /// dispatcher's next pass finds the branch at the start's tree, and
    /// records that set-aside all the same, once.
    #[test]
    fn a_set_aside_whose_record_failed_is_recorded_by_the_next_pass() {
        use crate::set_aside::{Done, set_aside};
        let db = Db::new("set_aside_retry");
        let (base, source, root, mirror, store) = world("set-aside-retry");
        let (agent, session) = db.task_on(&source, "set-aside-retry");
        let workspace = agent_with_work(&store, &agent, |directory| {
            std::fs::write(directory.join("work.txt"), "cancelled\n").expect("write");
        });
        store.unmount(&workspace).expect("its execution ended");
        let work = head(&mirror, &agent);
        let cancelled = |session: &str| crate::deliver::Cancelled {
            id: 7,
            mission_id: 1,
            agent_id: agent.clone(),
            session_id: session.to_string(),
            start: None,
            set_aside: None,
            told: false,
            closed_by: Some("task.cancelled".to_string()),
            reason: None,
            yielded: false,
            yield_reason: None,
        };
        let recorded = || {
            let mut client =
                postgres::Client::connect(&db.url, postgres::NoTls).expect("connection");
            client
                .batch_execute(&format!("set search_path to {}", db.schema))
                .expect("schema");
            client
                .query(
                    "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
                      where source_event_id = 'set-aside:7'",
                    &[],
                )
                .expect("the records")
                .iter()
                .map(|row| row.get::<_, String>(0))
                .collect::<Vec<_>>()
        };
        let error = set_aside(&db.store, &root, &cancelled("session_of_another"))
            .expect_err("the append is refused");
        assert!(error.contains("record the set-aside of task #7"), "{error}");
        let aside = head(&mirror, &agent);
        assert_ne!(aside, work, "the branch moved before the record");
        assert!(recorded().is_empty());

        let done = set_aside(&db.store, &root, &cancelled(&session)).expect("the next pass");
        assert_eq!(
            done,
            Done::SetAside {
                commit: aside.clone(),
                kept: work,
            }
        );
        assert_eq!(recorded(), std::slice::from_ref(&aside));
        // And a pass after that (the dispatcher then reads it back) adds none.
        set_aside(&db.store, &root, &cancelled(&session)).expect("again");
        assert_eq!(recorded(), std::slice::from_ref(&aside));
        assert_eq!(head(&mirror, &agent), aside, "no second commit");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// rev-e on a114a9e: a fork that fails every pass. Its first failure is
    /// recorded once (`task.fork_failed`, with the error); the Start waits
    /// until that record is the give-up bound old, then goes without the
    /// fork, from main, its prompt saying why.
    #[test]
    fn a_fork_that_keeps_failing_gives_up_and_starts_from_main() {
        use spawn_core::event::CheckpointReason;
        let db = Db::new("prior_fork_wedge");
        let (base, source, root, mirror, store) = world("prior-fork-wedge");
        let (first, second, _) = reassigned(&db, &store, &mirror, &source, |directory| {
            std::fs::write(directory.join("work.txt"), "half of it\n").expect("write");
        });
        // Its newest checkpoint names a commit no repository holds.
        let lost = "6".repeat(40);
        checkpoint_record(&db, &first, &lost, CheckpointReason::ToolCompleted, None);
        let mut order = start(&second);
        for _ in 0..2 {
            let held = crate::dispatch::fork_before_within(&db.store, &root, &mut order, 3_600_000)
                .expect_err("held");
            assert!(
                held.contains("Start waits") && held.contains(&lost),
                "{held}"
            );
        }
        assert_eq!(prompt(&order), "the task");
        let recorded = |db: &Db| -> Vec<String> {
            let mut client =
                postgres::Client::connect(&db.url, postgres::NoTls).expect("connection");
            client
                .batch_execute(&format!("set search_path to {}", db.schema))
                .expect("schema");
            client
                .query(
                    "select kind -> 'detail' -> 'record' ->> 'error' from spawn_events
                      where session_id = 'session_control'
                        and kind -> 'detail' -> 'record' ->> 'type' = 'task.fork_failed'",
                    &[],
                )
                .expect("records")
                .iter()
                .map(|row| row.get(0))
                .collect()
        };
        let errors = recorded(&db);
        assert_eq!(errors.len(), 1, "once per task");
        assert!(errors[0].contains(&lost), "{errors:?}");

        let line = crate::dispatch::fork_before_within(&db.store, &root, &mut order, 0)
            .expect("it gives up")
            .expect("said");
        assert!(line.contains("starts without"), "{line}");
        let told = prompt(&order);
        assert!(
            told.contains(&format!(
                "Spawn could not prepare your workspace from task #{}'s work (commit {lost})",
                first.0
            )) && told.contains("Your workspace starts from the source's main"),
            "{told}"
        );
        assert_eq!(recorded(&db).len(), 1);
        assert_eq!(crate::dispatch::fork_give_up_ms(None), 300_000);
        assert_eq!(crate::dispatch::fork_give_up_ms(Some(" 60 ")), 60_000);
        assert_eq!(crate::dispatch::fork_give_up_ms(Some("0")), 300_000);
        assert_eq!(
            crate::dispatch::fork_give_up_ms(Some("9223372036854775807")),
            i64::MAX,
            "an absurd setting saturates"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The fork when main conflicts with the work in a way markers cannot
    /// show (the work deleted README.md, main changed it): nothing is
    /// committed, the prompt names main's copy, and the Start's mount puts
    /// that copy in the workspace.
    #[test]
    fn a_reassigned_task_gets_main_copies_for_an_unmarked_conflict() {
        let db = Db::new("prior_fork_unmarked");
        let (base, source, root, mirror, store) = world("prior-fork-unmarked");
        let (_, second, _) = reassigned(&db, &store, &mirror, &source, |directory| {
            std::fs::remove_file(directory.join("README.md")).expect("delete");
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("README.md"), "one\ntwo (main)\nthree\n").expect("write");
        });
        let mut order = start(&second);
        crate::dispatch::fork_before(&db.store, &root, &mut order)
            .expect("the fork")
            .expect("it forked");
        let told = prompt(&order);
        assert!(
            told.contains("The source's main conflicts with it")
                && told.contains(".spawn/main")
                && told.contains("README.md"),
            "{told}"
        );
        assert!(!store.has_pending_merge(&second.1));
        let branch = BranchName::new(second.1.clone()).expect("branch");
        assert!(matches!(
            store.merge_main_clean(&branch).expect("the start's merge"),
            spawn_core::port::BaseMerge::Conflicted { marked: false, .. }
        ));
        let workspace = store.mount(&branch).expect("the start's mount");
        let directory = Path::new(workspace.path());
        assert_eq!(
            std::fs::read_to_string(directory.join(super::MAIN_COPIES).join("README.md"))
                .expect("main's copy"),
            "one\ntwo (main)\nthree\n"
        );
        assert!(directory.join("work.txt").exists(), "the work is there");
        let _ = std::fs::remove_dir_all(&base);
    }

    impl Drop for Db {
        fn drop(&mut self) {
            if let Ok(mut client) = postgres::Client::connect(&self.url, postgres::NoTls) {
                let _ =
                    client.batch_execute(&format!("drop schema if exists {} cascade", self.schema));
            }
        }
    }

    /// Issues #7, #8 and #12 through `merge_main` itself: with an input in
    /// flight in the worker's session, main is not merged (the workspace and
    /// the branch stay as they were); with none, every merge is recorded and
    /// published to the source, the one whose push lost the ref lock on the
    /// retry, and all of them run on one pusher, with one thread, for the
    /// source.
    #[test]
    fn merge_main_waits_for_a_busy_worker_and_publishes_every_merge_on_one_pusher() {
        use spawn_core::command::{CommandAction, Submission};
        use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
        use spawn_core::port::CommandQueue;
        let db = Db::new("merge_main");
        let (base, source, root, mirror, store) = world("merge-main");
        let (busy, busy_session) = db.task_on(&source, "busy");
        let (idle, idle_session) = db.task_on(&source, "idle");
        for agent in [&busy, &idle] {
            agent_with_work(&store, agent, |directory| {
                std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
            });
        }
        // A person types into the busy worker: the input is in flight.
        db.store
            .submit(
                &Principal::new("human").expect("principal"),
                &Submission {
                    agent_id: AgentId::new(busy.clone()).expect("agent"),
                    session_id: SessionId::new(busy_session.clone()).expect("session"),
                    idempotency_key: IdempotencyKey::new("human-1").expect("key"),
                    action: CommandAction::Input {
                        text: "run the tests".to_string(),
                        tags: Vec::new(),
                    },
                },
            )
            .expect("submit");
        let landing = |agent: &str| {
            fetch_pushed_into(
                &mirror,
                &source,
                agent,
                &format!("refs/spawn-integrate/{agent}"),
            )
            .expect("fetch")
        };
        let busy_head = head(&mirror, &busy);
        // The first merge's push loses the source's ref lock to another
        // push (issue #8): it is retried, and lands.
        let lock = Path::new(&source).join(format!("refs/heads/spawn/{idle}.lock"));
        std::fs::create_dir_all(lock.parent().expect("refs")).expect("refs");
        std::fs::write(&lock, "").expect("lock");
        for round in 0..3 {
            main_moves(&base, &source, &mirror, |clone| {
                std::fs::write(clone.join(format!("main-{round}.txt")), "from main\n")
                    .expect("write");
            });
            let merge = super::merge_main(&db.store, &root, &busy, &busy_session);
            assert!(matches!(merge, Merge::Busy(_)), "{merge:?}");
            assert_eq!(head(&mirror, &busy), busy_head, "no merge while it is busy");
            assert!(!root.join(&busy).join(format!("main-{round}.txt")).exists());

            let merge = super::merge_main(&db.store, &root, &idle, &idle_session);
            let Merge::Merged { commit, .. } = &merge else {
                panic!("an idle worker gets main: {merge:?}");
            };
            if round == 0 {
                assert_ne!(landing(&idle).as_deref(), Some(commit.as_str()));
                std::fs::remove_file(&lock).expect("the other push ends");
                let deadline = Instant::now() + Duration::from_secs(20);
                while landing(&idle).as_deref() != Some(commit.as_str())
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            assert_eq!(
                landing(&idle).as_deref(),
                Some(commit.as_str()),
                "published (round {round})"
            );
            assert_eq!(
                db.store
                    .latest_checkpoint(&idle)
                    .expect("checkpoint")
                    .as_deref(),
                Some(commit.as_str()),
                "recorded"
            );
        }
        assert_eq!(
            crate::push::shared(&source).threads(),
            1,
            "one pusher thread for the source (it retried the failed push), not one per merge"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #7, at the dispatcher: a task about to be typed into a live
    /// worker whose merge of main finds the worker busy (a person's input in
    /// flight in its session) is not sent this pass: nothing is merged, the
    /// order goes nowhere, and the next pass tries again by the same rules.
    #[test]
    fn the_dispatcher_holds_a_delivery_whose_merge_finds_the_worker_busy() {
        use spawn_core::command::{CommandAction, Submission};
        use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
        use spawn_core::port::CommandQueue;
        let db = Db::new("delivery_hold");
        let (base, source, root, mirror, store) = world("delivery-hold");
        let (agent, session) = db.task_on(&source, "held");
        agent_with_work(&store, &agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        let before = head(&mirror, &agent);
        main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });
        db.store
            .submit(
                &Principal::new("human").expect("principal"),
                &Submission {
                    agent_id: AgentId::new(agent.clone()).expect("agent"),
                    session_id: SessionId::new(session.clone()).expect("session"),
                    idempotency_key: IdempotencyKey::new("human-1").expect("key"),
                    action: CommandAction::Input {
                        text: "one more thing".to_string(),
                        tags: Vec::new(),
                    },
                },
            )
            .expect("submit");
        let mut order = crate::dispatch::Order {
            agent_id: agent.clone(),
            session_id: session.clone(),
            key: "deliver-1-0".to_string(),
            action: CommandAction::Input {
                text: "the task".to_string(),
                tags: Vec::new(),
            },
            task: Some(1),
        };
        let held = crate::dispatch::merge_main_before(&db.store, &root, &mut order)
            .expect_err("the delivery is held");
        assert!(
            held.contains("waits") && held.contains("in flight"),
            "{held}"
        );
        assert_eq!(head(&mirror, &agent), before, "nothing merged");
        assert!(!root.join(&agent).join("added.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
