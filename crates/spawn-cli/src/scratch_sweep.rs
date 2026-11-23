//! `spawn scratch sweep` (objective 145): the build output of landed
//! objectives goes, and nothing else.
//!
//! `spawn scratch sweep [--excuse <pid>:<start ticks>]... [--apply
//! --expect-digest <digest>]` reads the claimed folders the storage line's
//! scratch walk finds (crate::serve::core_lines::claimed_scratch: the Core
//! lines file's `storage.scratch_roots`, claims at a root's top level or one
//! below, each folder rechecked against every name and place the walk never
//! counts), and the store's landings of their objectives, once
//! (`spawn_postgres::verifications::CLAIMED_LANDED`). It never decides from
//! the unowned list. A build path an owner file names goes only when:
//! - its claim's lease has ended;
//! - its mission and objective have a `task.landed` (the fleet's code
//!   landing);
//! - it is a build path (relative, plain parts, last part `target`);
//! - every component under the folder is a real directory, not a symlink,
//!   on the folder's device;
//! - it holds a CACHEDIR.TAG, a regular file starting with the standard's
//!   signature;
//! - nothing under it is a transcript (`*.jsonl`) or on another device;
//! - no process uses it ([`in_use`]): no cwd, root, exe, open file or
//!   mapping at or under it; a process of the path's owner whose /proc
//!   cannot be read refuses it, but for an [`EXCUSED`] name started before
//!   the claim, or a pair given by `--excuse`.
//!
//! Print only unless `--apply` is given. Every run first lists the user's
//! unreadable processes (`unreadable` or `excuse <pid>:<start ticks>
//! <comm>`); an `--excuse` that is not one of them refuses the run. The
//! print-only list ends with its digest, the sha256 of each `would-remove`
//! row (path, bytes, owner, the directories' `<dev>:<ino>`, the claim's
//! sha256) and of the excuses, sorted. `--apply` recomputes the list and
//! acts only when its digest is the one given; it then judges each path
//! again just before removal ([`rejudge`]) and removes the very directory
//! judged, from file descriptors ([`remove`]): no name is resolved again,
//! no symlink followed, nothing renamed and no working directory changed.
//!
//! It also reclaims the build output of finished ephemeral workers: a
//! workspace `<root>/agent_worker_*` (`SPAWN_WORKSPACE_ROOT`, its folder
//! name the worker's agent id) whose `target` goes only when:
//! - the store knows the agent's tasks, each ephemeral and each in a
//!   terminal state ([`TERMINAL`]: `accepted`, `landed` or `cancelled`);
//! - no execution of any of their sessions is still running;
//! - every rule above from "every component" on holds (the same device, no
//!   symlink, the CACHEDIR.TAG signature, no transcript, nothing in use),
//!   and nothing under it is a git repository or worktree (a `.git`).
//!
//! Only `target` is ever removed: never the workspace, its sources, a git
//! worktree or branch, nor anything under the transcript archive
//! (`<root>/.transcripts`, no worker's workspace). `--apply` reads the
//! worker's tasks again just before removal; changed, it stays.
//!
//! What it relies on: a claim's lease is no lock. The sweep relies on the
//! lease having ended and on no process using the path when it is judged
//! again; a process that writes into the judged directory after that, against
//! an ended lease, loses what it writes there. Nothing outside that directory
//! is ever reached.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

use crate::scratch_owner::{Claim, build_path_ok, claim_in};

/// The unreadable processes excused by name, when they started before the
/// claim: the login's systemd helper.
pub(crate) const EXCUSED: [&str; 1] = ["(sd-pam)"];

/// The cache directory tagging standard's signature.
const TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// The task states a worker's workspace may be reclaimed in: its work is
/// done with, whether it landed or not.
pub(crate) const TERMINAL: [&str; 3] = ["accepted", "landed", "cancelled"];

/// An ephemeral worker's workspace name starts so (its agent id).
pub(crate) const WORKER_PREFIX: &str = "agent_worker_";

/// The one build path of a worker's workspace.
const WORKER_BUILD: &str = "target";

/// Clock ticks per second of /proc's start times.
const USER_HZ: u64 = 100;

const USAGE: &str = "usage: spawn scratch sweep [--excuse <pid>:<start ticks>]... \
                     [--apply --expect-digest <digest>]";

/// The directories opened along the way: never through a symlink.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Where a test may act between two steps of an `--apply` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The list's digest matched; no path is judged again yet.
    Digested,
    /// A path's re-judgement checked its components and runs its in-use
    /// proof.
    InUse,
    /// The removal opened the build path's directory and checked it.
    Opened,
}

/// The arguments: the excuses, and the digest `--apply` acts on.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    excuses: Vec<(u32, u64)>,
    expect: Option<String>,
}

/// `<pid>:<start ticks>`, digits only.
fn pair(text: &str) -> Option<(u32, u64)> {
    let (pid, ticks) = text.split_once(':')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(pid) || !digits(ticks) {
        return None;
    }
    Some((pid.parse().ok()?, ticks.parse().ok()?))
}

fn args(words: &[String]) -> Result<Args, String> {
    let mut apply = false;
    let mut parsed = Args::default();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--apply" if !apply => apply = true,
            "--expect-digest" if parsed.expect.is_none() => {
                let value = words
                    .next()
                    .ok_or_else(|| format!("--expect-digest needs a value\n{USAGE}"))?;
                parsed.expect = Some(value.clone());
            }
            "--excuse" => {
                let value = words
                    .next()
                    .ok_or_else(|| format!("--excuse needs a value\n{USAGE}"))?;
                parsed
                    .excuses
                    .push(pair(value).ok_or_else(|| {
                        format!("--excuse is <pid>:<start ticks>, not '{value}'")
                    })?);
            }
            "--apply" | "--expect-digest" => return Err(format!("{word} is given twice")),
            other => return Err(format!("unexpected argument {other}\n{USAGE}")),
        }
    }
    if apply != parsed.expect.is_some() {
        return Err(
            "--apply and --expect-digest go together: --apply acts only on the list read, by \
             the digest it ends with"
                .to_string(),
        );
    }
    parsed.excuses.sort_unstable();
    parsed.excuses.dedup();
    Ok(parsed)
}

/// `spawn scratch sweep`.
pub(crate) fn command(words: &[String]) -> Result<i32, String> {
    let args = args(words)?;
    let read = crate::serve::core_lines::claimed_scratch(crate::verify::now_ms())
        .map_err(|why| format!("{why}; nothing removed"))?;
    let root = crate::env_value("SPAWN_WORKSPACE_ROOT")
        .map(|root| spawn_host::canonical_root(Path::new(&root)));
    let found = root.as_deref().map(worker_folders).unwrap_or_default();
    let store = if found.is_empty() {
        None
    } else {
        Some(crate::open_store().map_err(|why| {
            format!(
                "the store cannot be opened: {}; nothing removed",
                crate::alerts::scrub(&why)
            )
        })?)
    };
    let reread = |agent: &str| -> Result<Worker, String> {
        let store = store
            .as_ref()
            .ok_or_else(|| "the store is not open".to_string())?;
        worker_of(&label_facts(store)?, agent, |session| {
            session_runs(store, session)
        })
    };
    let mut workers = Vec::new();
    if let Some(store) = &store {
        let facts = label_facts(store).map_err(|why| format!("{why}; nothing removed"))?;
        for folder in found {
            let agent = folder_agent(&folder).unwrap_or_default().to_string();
            let worker = worker_of(&facts, &agent, |session| session_runs(store, session))
                .map_err(|why| format!("{why}; nothing removed"))?;
            workers.push((folder, worker));
        }
    }
    let inputs = Inputs {
        folders: &read.folders,
        landed: &read.landed,
        workers: &workers,
        reread: &reread,
        proc_root: Path::new("/proc"),
        uid: spawn_host::process::current_uid(),
    };
    if root.is_none() {
        println!("workers\tnot read: SPAWN_WORKSPACE_ROOT is not set");
    }
    sweep(&inputs, &args, &|_, _| {}, &mut |line| println!("{line}"))
}

/// The agent id a worker's workspace `folder` is named for, when it is one.
fn folder_agent(folder: &Path) -> Option<&str> {
    folder
        .file_name()?
        .to_str()
        .filter(|name| name.starts_with(WORKER_PREFIX) && name.len() > WORKER_PREFIX.len())
}

/// The ephemeral workers' workspaces under `root` that hold a `target`
/// (whatever it is: a rule says why one stays): each a real directory, not
/// a symlink, named `agent_worker_*`, sorted. The transcript archive is
/// never one.
pub(crate) fn worker_folders(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| folder_agent(path).is_some())
        .filter(|path| path.as_path() != crate::storage::archive_path(root))
        .filter(|path| {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        })
        .filter(|path| std::fs::symlink_metadata(path.join(WORKER_BUILD)).is_ok())
        .collect();
    found.sort();
    found
}

/// One task of a worker, as the store has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerTask {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) state: String,
    pub(crate) session: String,
    pub(crate) created_ms: i64,
}

/// An ephemeral worker as the store has it: its tasks, and the sessions of
/// theirs with an execution still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Worker {
    pub(crate) agent: String,
    pub(crate) tasks: Vec<WorkerTask>,
    pub(crate) live: Vec<String>,
}

impl Worker {
    /// What the digest holds of it: every fact it is judged on.
    fn line(&self) -> String {
        let tasks: Vec<serde_json::Value> = self
            .tasks
            .iter()
            .map(|task| {
                serde_json::json!({
                    "id": task.id, "kind": task.kind, "state": task.state,
                    "session": task.session, "created_ms": task.created_ms,
                })
            })
            .collect();
        serde_json::json!({"agent": self.agent, "tasks": tasks, "live": self.live}).to_string()
    }

    /// The earliest of its tasks' creation: what an unreadable process
    /// excused by name must have started before.
    fn created_ms(&self) -> i64 {
        self.tasks
            .iter()
            .map(|task| task.created_ms)
            .min()
            .unwrap_or(i64::MIN)
    }

    /// Why its workspace's build output stays, if it does: no task known,
    /// a task not ephemeral or not in a terminal state, or an execution
    /// still running.
    fn stays(&self) -> Option<String> {
        if self.tasks.is_empty() {
            return Some("the store knows no task of it".to_string());
        }
        if let Some(task) = self.tasks.iter().find(|task| task.kind != "ephemeral") {
            return Some(format!("task {} is {}, not ephemeral", task.id, task.kind));
        }
        if let Some(task) = self
            .tasks
            .iter()
            .find(|task| !TERMINAL.contains(&task.state.as_str()))
        {
            return Some(format!("task {} is {}", task.id, task.state));
        }
        self.live
            .first()
            .map(|session| format!("an execution of {session} still runs"))
    }
}

/// The worker `agent` from the dashboard's label facts (`facts`, as
/// `PostgresStore::label_facts` reads them: every session's latest task and
/// every task not yet terminal), each session's running executions told by
/// `runs`.
pub(crate) fn worker_of(
    facts: &serde_json::Value,
    agent: &str,
    runs: impl Fn(&str) -> Result<bool, String>,
) -> Result<Worker, String> {
    let missions = facts["missions"]
        .as_array()
        .ok_or_else(|| "the store's label facts have no missions".to_string())?;
    let mut tasks: Vec<WorkerTask> = Vec::new();
    for task in missions
        .iter()
        .filter_map(|mission| mission["tasks"].as_array())
        .flatten()
        .filter(|task| task["agent_id"] == agent)
    {
        let Some(id) = task["id"].as_i64() else {
            return Err(format!("a task of {agent} has no id"));
        };
        if tasks.iter().any(|known| known.id == id) {
            continue;
        }
        let text = |field: &str| task[field].as_str().unwrap_or_default().to_string();
        tasks.push(WorkerTask {
            id,
            kind: text("kind"),
            state: text("state"),
            session: text("session_id"),
            created_ms: task["created_at_ms"].as_i64().unwrap_or(i64::MIN),
        });
    }
    tasks.sort_by_key(|task| task.id);
    let mut live: Vec<String> = Vec::new();
    let mut asked: Vec<&str> = Vec::new();
    for task in &tasks {
        if task.session.is_empty() || asked.contains(&task.session.as_str()) {
            continue;
        }
        asked.push(&task.session);
        if runs(&task.session)? {
            live.push(task.session.clone());
        }
    }
    Ok(Worker {
        agent: agent.to_string(),
        tasks,
        live,
    })
}

/// The store's label facts ([`worker_of`]).
fn label_facts(store: &spawn_postgres::PostgresStore) -> Result<serde_json::Value, String> {
    store.label_facts().map_err(|error| {
        format!(
            "the store's tasks cannot be read: {}",
            crate::alerts::scrub(&error.to_string())
        )
    })
}

/// Whether an execution of `session` still runs, by the store.
fn session_runs(store: &spawn_postgres::PostgresStore, session: &str) -> Result<bool, String> {
    let id = spawn_core::id::SessionId::new(session.to_string())
        .map_err(|error| format!("session {session}: {error}"))?;
    let executions = store.executions(&id).map_err(|error| {
        format!(
            "the executions of {session} cannot be read: {}",
            crate::alerts::scrub(&error.to_string())
        )
    })?;
    Ok(executions
        .iter()
        .any(|execution| execution.ended_at.is_none()))
}

/// What a sweep decides from: the claimed folders, their landed pairs, the
/// workers' workspaces, where /proc is and the user running it.
pub(crate) struct Inputs<'a> {
    pub(crate) folders: &'a [(PathBuf, Claim)],
    pub(crate) landed: &'a [(i64, i64)],
    /// The ephemeral workers' workspaces holding a `target`, each with its
    /// worker as the store had it when listed.
    pub(crate) workers: &'a [(PathBuf, Worker)],
    /// The worker of an agent as the store has it now: read again just
    /// before its build output goes.
    pub(crate) reread: &'a dyn Fn(&str) -> Result<Worker, String>,
    pub(crate) proc_root: &'a Path,
    pub(crate) uid: u32,
}

/// Whose a build path is: a claimed folder's, or a worker's workspace's.
#[derive(Debug, Clone, Copy)]
enum Owner<'a> {
    Claim(&'a Claim),
    Worker(&'a Worker),
}

/// One build path of one claimed folder or worker's workspace.
struct Row<'a> {
    folder: &'a Path,
    owner: Owner<'a>,
    path: PathBuf,
    parts: Vec<&'a OsStr>,
    build: &'a str,
}

impl Row<'_> {
    fn who(&self) -> String {
        match self.owner {
            Owner::Claim(claim) => format!("{} {}", claim.agent, claim.objective),
            Owner::Worker(worker) => {
                let tasks: Vec<String> = worker
                    .tasks
                    .iter()
                    .map(|task| format!("task {} {}", task.id, task.state))
                    .collect();
                if tasks.is_empty() {
                    worker.agent.clone()
                } else {
                    format!("{} {}", worker.agent, tasks.join(", "))
                }
            }
        }
    }

    /// The digest's word for its owner: the sha256 of the claim's line, or
    /// of the worker's facts.
    fn owned(&self) -> String {
        match self.owner {
            Owner::Claim(claim) => {
                format!("claim {}", hex(&Sha256::digest(claim.line().as_bytes())))
            }
            Owner::Worker(worker) => {
                format!("worker {}", hex(&Sha256::digest(worker.line().as_bytes())))
            }
        }
    }

    /// When its owner began: what an unreadable process excused by name must
    /// have started before.
    fn created_ms(&self) -> i64 {
        match self.owner {
            Owner::Claim(claim) => claim.created_ms,
            Owner::Worker(worker) => worker.created_ms(),
        }
    }

    fn stays(&self, why: &str) -> String {
        format!("stays\t{}\t{}: {why}", self.path.display(), self.who())
    }
}

/// A path judged to go: its bytes, the `(dev, ino)` of the folder and of
/// each component, the owner's uid, and the processes excused by name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Judged {
    bytes: u64,
    ids: Vec<(u64, u64)>,
    excused: Vec<(u32, String)>,
}

fn would_remove(row: &Row, judged: &Judged) -> String {
    let ids: Vec<String> = judged
        .ids
        .iter()
        .map(|(dev, ino)| format!("{dev}:{ino}"))
        .collect();
    format!(
        "would-remove\t{}\t{}\t{}\tids {}\t{}",
        row.path.display(),
        judged.bytes,
        row.who(),
        ids.join(","),
        row.owned()
    )
}

fn excused_lines(row: &Row, judged: &Judged) -> Vec<String> {
    judged
        .excused
        .iter()
        .map(|(pid, comm)| {
            format!(
                "excused\t{}\tprocess {pid} {comm}: cannot be read, started before the {}",
                row.path.display(),
                match row.owner {
                    Owner::Claim(_) => "claim",
                    Owner::Worker(_) => "task",
                }
            )
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The list's digest: the sha256 of each `would-remove` row and each
/// excuse, sorted, each with its newline (an empty list: the empty
/// string's).
fn digest(rows: &[String], excuses: &[(u32, u64)]) -> String {
    let mut lines: Vec<String> = rows.to_vec();
    lines.extend(
        excuses
            .iter()
            .map(|(pid, ticks)| format!("excuse\t{pid}:{ticks}")),
    );
    lines.sort();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

/// The sweep over `inputs`: print only without `args.expect`, else the
/// list's digest must be it. `hook` is where a test acts between steps.
fn sweep(
    inputs: &Inputs,
    args: &Args,
    hook: &dyn Fn(Stage, &Path),
    out: &mut dyn FnMut(String),
) -> Result<i32, String> {
    let procs = snapshot(inputs.proc_root).map_err(|why| format!("{why}; nothing removed"))?;
    let unreadable: Vec<&Process> = procs
        .list
        .iter()
        .filter(|process| process.uses.is_none() && process.uid.is_none_or(|uid| uid == inputs.uid))
        .collect();
    for &(pid, ticks) in &args.excuses {
        if !unreadable
            .iter()
            .any(|process| process.pid == pid && process.start == Some(ticks))
        {
            return Err(format!(
                "--excuse {pid}:{ticks} is no unreadable process of yours running now; nothing \
                 removed"
            ));
        }
    }
    let mut first: Vec<String> = unreadable
        .iter()
        .map(|process| {
            let start = process
                .start
                .map_or("?".to_string(), |start| start.to_string());
            let excused = process
                .start
                .is_some_and(|start| args.excuses.contains(&(process.pid, start)));
            let word = if excused { "excuse" } else { "unreadable" };
            format!("{word}\t{}:{start}\t{}", process.pid, process.comm)
        })
        .collect();
    // The excused set first, beside the list.
    first.sort_by_key(|line| !line.starts_with("excuse\t"));
    let rows = rows(inputs.folders, inputs.workers);
    let now = crate::verify::now_ms();
    let listed: Vec<Result<Judged, String>> = rows
        .iter()
        .map(|row| {
            let (judged, owner) = examine(row, inputs.landed, now)?;
            match in_use(&row.path, owner, row.created_ms(), &procs, &args.excuses) {
                Proof::Used(why) => Err(why),
                Proof::Free(by_name) => Ok(Judged {
                    excused: by_name,
                    ..judged
                }),
            }
        })
        .collect();
    let would: Vec<String> = rows
        .iter()
        .zip(&listed)
        .filter_map(|(row, judged)| Some(would_remove(row, judged.as_ref().ok()?)))
        .collect();
    let digest = digest(&would, &args.excuses);
    let Some(expect) = &args.expect else {
        first.into_iter().for_each(&mut *out);
        let mut bytes = 0_u64;
        for (row, judged) in rows.iter().zip(&listed) {
            match judged {
                Ok(judged) => {
                    excused_lines(row, judged).into_iter().for_each(&mut *out);
                    out(would_remove(row, judged));
                    bytes = bytes.saturating_add(judged.bytes);
                }
                Err(why) => out(row.stays(why)),
            }
        }
        out(format!(
            "would remove (print only; nothing removed) {} build path(s), {bytes} bytes",
            would.len()
        ));
        out(format!("digest\t{digest}"));
        return Ok(0);
    };
    if *expect != digest {
        return Err(format!(
            "the list is not the one read (its digest is now {digest}); nothing removed"
        ));
    }
    first.into_iter().for_each(&mut *out);
    hook(Stage::Digested, Path::new(""));
    Ok(apply(&rows, &listed, inputs, &args.excuses, hook, out))
}

/// `--apply` on the list read: each path judged again just before it
/// goes, and removed only as it was listed. 1 when a removal failed.
fn apply(
    rows: &[Row],
    listed: &[Result<Judged, String>],
    inputs: &Inputs,
    excuses: &[(u32, u64)],
    hook: &dyn Fn(Stage, &Path),
    out: &mut dyn FnMut(String),
) -> i32 {
    let (mut count, mut bytes, mut failed) = (0_u64, 0_u64, false);
    for (row, judged) in rows.iter().zip(listed) {
        let judged = match judged {
            Ok(judged) => judged,
            Err(why) => {
                out(row.stays(why));
                continue;
            }
        };
        let again = match rejudge(row, inputs, excuses, hook) {
            Ok(again) => again,
            Err(why) => {
                out(row.stays(&why));
                continue;
            }
        };
        excused_lines(row, &again).into_iter().for_each(&mut *out);
        if again.bytes != judged.bytes || again.ids != judged.ids {
            out(row.stays("changed since the list"));
            continue;
        }
        match remove(row, &judged.ids, hook) {
            Ok(()) => {
                out(format!(
                    "removed\t{}\t{}\t{}",
                    row.path.display(),
                    judged.bytes,
                    row.who()
                ));
                count += 1;
                bytes = bytes.saturating_add(judged.bytes);
            }
            Err(Removal::PathChanged) => out(row.stays("its path changed")),
            Err(Removal::Failed(why)) => {
                failed = true;
                out(format!(
                    "failed\t{}\t{}: {why}",
                    row.path.display(),
                    row.who()
                ));
            }
        }
    }
    out(format!("removed {count} build path(s), {bytes} bytes"));
    i32::from(failed)
}

/// Each build path of each folder, once per folder, in order; then each
/// worker's workspace's `target`, unless a claimed folder's row is that
/// very path.
fn rows<'a>(folders: &'a [(PathBuf, Claim)], workers: &'a [(PathBuf, Worker)]) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for (folder, claim) in folders {
        let mut seen: Vec<&str> = Vec::new();
        for build in &claim.build {
            if seen.contains(&build.as_str()) {
                continue;
            }
            seen.push(build);
            rows.push(Row {
                folder,
                owner: Owner::Claim(claim),
                path: folder.join(build),
                parts: Path::new(build)
                    .components()
                    .map(Component::as_os_str)
                    .collect(),
                build,
            });
        }
    }
    for (folder, worker) in workers {
        let path = folder.join(WORKER_BUILD);
        if rows.iter().any(|row| row.path == path) {
            continue;
        }
        rows.push(Row {
            folder,
            owner: Owner::Worker(worker),
            path,
            parts: vec![OsStr::new(WORKER_BUILD)],
            build: WORKER_BUILD,
        });
    }
    rows
}

/// A path judged again just before it goes: its owner file still holds
/// the very claim listed, and every rule holds, its in-use proof read
/// afresh.
fn rejudge(
    row: &Row,
    inputs: &Inputs,
    excuses: &[(u32, u64)],
    hook: &dyn Fn(Stage, &Path),
) -> Result<Judged, String> {
    match row.owner {
        Owner::Claim(claim) => {
            if claim_in(row.folder).as_ref() != Some(claim) {
                return Err("its claim changed".to_string());
            }
        }
        Owner::Worker(worker) => match (inputs.reread)(&worker.agent) {
            Ok(now) if now == *worker => {}
            Ok(_) => return Err("its tasks changed".to_string()),
            Err(why) => return Err(format!("its tasks cannot be read again: {why}")),
        },
    }
    let (judged, owner) = examine(row, inputs.landed, crate::verify::now_ms())?;
    hook(Stage::InUse, &row.path);
    let procs = snapshot(inputs.proc_root).map_err(|why| format!("in use unknown: {why}"))?;
    match in_use(&row.path, owner, row.created_ms(), &procs, excuses) {
        Proof::Used(why) => Err(why),
        Proof::Free(by_name) => Ok(Judged {
            excused: by_name,
            ..judged
        }),
    }
}

/// Every rule but the in-use proof, at `now`: the judged path's bytes and
/// ids, and its owner's uid; else why it stays.
fn examine(row: &Row, landed: &[(i64, i64)], now: i64) -> Result<(Judged, u32), String> {
    match row.owner {
        Owner::Claim(claim) => {
            if claim.lease_until_ms > now {
                return Err("lease runs".to_string());
            }
            if !claim.pair().is_some_and(|pair| landed.contains(&pair)) {
                return Err("not landed".to_string());
            }
        }
        Owner::Worker(worker) => {
            if let Some(why) = worker.stays() {
                return Err(why);
            }
            if row
                .path
                .components()
                .any(|part| part.as_os_str() == OsStr::new(crate::storage::ARCHIVE))
            {
                return Err("under the transcript archive".to_string());
            }
        }
    }
    if !build_path_ok(row.build) {
        return Err("not a build path".to_string());
    }
    let folder = std::fs::symlink_metadata(row.folder)
        .map_err(|error| format!("{}: {error}", row.folder.display()))?;
    if folder.file_type().is_symlink() || !folder.is_dir() {
        return Err("its path changed".to_string());
    }
    let dev = folder.dev();
    let mut ids = vec![(dev, folder.ino())];
    let mut at = row.folder.to_path_buf();
    let mut owner = folder.uid();
    for part in &row.parts {
        at.push(part);
        let metadata = match std::fs::symlink_metadata(&at) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("not there".to_string());
            }
            Err(error) => return Err(format!("{}: {error}", at.display())),
        };
        if metadata.file_type().is_symlink() {
            return Err(format!("{} is a symlink", at.display()));
        }
        if !metadata.is_dir() {
            return Err(format!("{} is not a directory", at.display()));
        }
        if metadata.dev() != dev {
            return Err(format!("{} is on another device", at.display()));
        }
        ids.push((metadata.dev(), metadata.ino()));
        owner = metadata.uid();
    }
    if !tagged(&at) {
        return Err("no CACHEDIR.TAG".to_string());
    }
    let bytes = size(&at, dev)?;
    Ok((
        Judged {
            bytes,
            ids,
            excused: Vec::new(),
        },
        owner,
    ))
}

/// Whether `dir` holds a CACHEDIR.TAG: a regular file (not a symlink)
/// starting with the standard's signature.
fn tagged(dir: &Path) -> bool {
    let tag = dir.join("CACHEDIR.TAG");
    if !std::fs::symlink_metadata(&tag).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut head = Vec::new();
    std::fs::File::open(&tag)
        .and_then(|file| file.take(TAG_SIGNATURE.len() as u64).read_to_end(&mut head))
        .is_ok_and(|_| head == TAG_SIGNATURE)
}

/// The regular files' bytes under `dir`, no symlink followed; why it stays
/// when anything there is a transcript, a git repository's or worktree's
/// `.git`, or on another device than `dev`.
fn size(dir: &Path, dev: u64) -> Result<u64, String> {
    let mut bytes = 0_u64;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let children =
            std::fs::read_dir(&next).map_err(|error| format!("{}: {error}", next.display()))?;
        for child in children {
            let path = child
                .map_err(|error| format!("{}: {error}", next.display()))?
                .path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                return Err(format!("{} is a transcript", path.display()));
            }
            if path.file_name().is_some_and(|name| name == ".git") {
                return Err(format!("{} is a git repository's", path.display()));
            }
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            if metadata.dev() != dev {
                return Err(format!("{} is on another device", path.display()));
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok(bytes)
}

/// What a process has at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cwd,
    Root,
    Exe,
    File,
    Mapping,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Cwd => "cwd",
            Kind::Root => "root",
            Kind::Exe => "exe",
            Kind::File => "open file",
            Kind::Mapping => "mapping",
        }
    }
}

/// One process as /proc shows it: `uses` is `None` when its cwd, root, exe,
/// open files or mappings cannot be read.
#[derive(Debug, Clone)]
struct Process {
    pid: u32,
    uid: Option<u32>,
    start: Option<u64>,
    comm: String,
    uses: Option<Vec<(Kind, PathBuf)>>,
}

/// Every process under a /proc, by pid, and its boot time.
#[derive(Debug, Clone)]
struct Procs {
    btime: Option<u64>,
    list: Vec<Process>,
}

fn gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

/// The processes under `root` (/proc) now.
fn snapshot(root: &Path) -> Result<Procs, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("{} cannot be read: {error}", root.display()))?;
    let btime = std::fs::read_to_string(root.join("stat"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok())
        });
    let mut list: Vec<Process> = entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            process(&entry.path(), pid)
        })
        .collect();
    list.sort_by_key(|process| process.pid);
    Ok(Procs { btime, list })
}

/// The process `pid` at `dir`; `None` when it is gone.
fn process(dir: &Path, pid: u32) -> Option<Process> {
    let uid = match std::fs::read_to_string(dir.join("status")) {
        Ok(text) => text.lines().find_map(|line| {
            line.strip_prefix("Uid:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        }),
        Err(error) if gone(&error) => return None,
        Err(_) => None,
    };
    let start = std::fs::read_to_string(dir.join("stat"))
        .ok()
        .and_then(|stat| spawn_host::process::start_time_of(&stat));
    let comm = std::fs::read_to_string(dir.join("comm")).map_or_else(
        |_| "?".to_string(),
        |comm| comm.trim_end_matches('\n').to_string(),
    );
    let uses = uses(dir);
    if uses.is_none() && std::fs::symlink_metadata(dir).is_err() {
        return None;
    }
    Some(Process {
        pid,
        uid,
        start,
        comm,
        uses,
    })
}

/// What the process at `dir` has open: `None` when any of it cannot be
/// read (a thing not there is no use).
fn uses(dir: &Path) -> Option<Vec<(Kind, PathBuf)>> {
    let mut found = Vec::new();
    for (name, kind) in [("cwd", Kind::Cwd), ("root", Kind::Root), ("exe", Kind::Exe)] {
        match std::fs::read_link(dir.join(name)) {
            Ok(path) => found.push((kind, path)),
            Err(error) if gone(&error) => {}
            Err(_) => return None,
        }
    }
    match std::fs::read_dir(dir.join("fd")) {
        Ok(fds) => {
            for fd in fds.flatten() {
                match std::fs::read_link(fd.path()) {
                    Ok(path) => found.push((Kind::File, path)),
                    Err(error) if gone(&error) => {}
                    Err(_) => return None,
                }
            }
        }
        Err(error) if gone(&error) => {}
        Err(_) => return None,
    }
    match std::fs::read(dir.join("maps")) {
        Ok(text) => {
            for line in text.split(|&byte| byte == b'\n') {
                if let Some(path) = mapped(line) {
                    found.push((Kind::Mapping, path));
                }
            }
        }
        Err(error) if gone(&error) => {}
        Err(_) => return None,
    }
    Some(found)
}

/// A maps line's path: what follows its five fields, when absolute.
fn mapped(line: &[u8]) -> Option<PathBuf> {
    let mut rest = line;
    for _ in 0..5 {
        let start = rest.iter().position(|byte| *byte != b' ')?;
        rest = &rest[start..];
        let end = rest.iter().position(|byte| *byte == b' ')?;
        rest = &rest[end..];
    }
    let start = rest.iter().position(|byte| *byte != b' ')?;
    let path = &rest[start..];
    path.starts_with(b"/")
        .then(|| PathBuf::from(OsStr::from_bytes(path)))
}

/// Whether `seen` (as /proc names it, " (deleted)" stripped) is `path` or
/// under it, compared by component.
fn under(seen: &Path, path: &Path) -> bool {
    let bytes = seen.as_os_str().as_bytes();
    let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    Path::new(OsStr::from_bytes(bytes)).starts_with(path)
}

/// The in-use proof's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Proof {
    /// No process uses it; the processes excused by name.
    Free(Vec<(u32, String)>),
    /// Why it stays.
    Used(String),
}

/// Whether a process uses `path` (owned by `owner`, claimed at
/// `created_ms`): a definite use (lowest pid) ahead of an unknown one; an
/// unreadable process of the owner is unknown but for an [`EXCUSED`] name
/// started before the claim, or an exact pair of `excuses`. Unreadable
/// processes of other users are not counted.
fn in_use(
    path: &Path,
    owner: u32,
    created_ms: i64,
    procs: &Procs,
    excuses: &[(u32, u64)],
) -> Proof {
    let mut unknown = None;
    let mut by_name = Vec::new();
    for process in &procs.list {
        let Some(uses) = &process.uses else {
            if process.uid.is_some_and(|uid| uid != owner) {
                continue;
            }
            if process
                .start
                .is_some_and(|start| excuses.contains(&(process.pid, start)))
            {
                continue;
            }
            let before = process
                .start
                .zip(procs.btime)
                .is_some_and(|(start, btime)| started_ms(btime, start) < created_ms);
            if before && EXCUSED.contains(&process.comm.as_str()) {
                by_name.push((process.pid, process.comm.clone()));
                continue;
            }
            unknown.get_or_insert_with(|| {
                format!(
                    "in use unknown: process {} ({}) of its owner cannot be read",
                    process.pid, process.comm
                )
            });
            continue;
        };
        if let Some((kind, _)) = uses.iter().find(|(_, seen)| under(seen, path)) {
            return Proof::Used(format!("in use: process {}'s {}", process.pid, kind.word()));
        }
    }
    unknown.map_or(Proof::Free(by_name), Proof::Used)
}

/// When a process started, in ms since the epoch: the boot time plus its
/// start ticks.
fn started_ms(btime: u64, ticks: u64) -> i64 {
    let ms = btime
        .saturating_mul(1000)
        .saturating_add(ticks.saturating_mul(1000 / USER_HZ));
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// Why a removal did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Removal {
    /// Something changed before the build path's directory was opened:
    /// nothing removed.
    PathChanged,
    /// Stopped part way: why.
    Failed(String),
}

fn id_of(stat: &rustix::fs::Stat) -> (u64, u64) {
    (stat.st_dev, stat.st_ino)
}

fn open_dir(at: impl AsFd, name: &OsStr) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(at, name, DIR_FLAGS, Mode::empty())
}

fn fstat_is(fd: impl AsFd, id: (u64, u64)) -> bool {
    rustix::fs::fstat(fd).is_ok_and(|stat| id_of(&stat) == id)
}

/// Remove the very directory judged, `ids` its folder's and components'
/// `(dev, ino)`: each opened through the one before, never through a
/// symlink, each as judged; its contents removed from its own fd; last,
/// its name, while that name still holds it.
fn remove(row: &Row, ids: &[(u64, u64)], hook: &dyn Fn(Stage, &Path)) -> Result<(), Removal> {
    let (Some(&folder_id), Some(&build_id), Some(last)) =
        (ids.first(), ids.last(), row.parts.last())
    else {
        return Err(Removal::PathChanged);
    };
    if ids.len() != row.parts.len() + 1 {
        return Err(Removal::PathChanged);
    }
    let folder = open_dir(CWD, row.folder.as_os_str()).map_err(|_| Removal::PathChanged)?;
    if !fstat_is(&folder, folder_id) {
        return Err(Removal::PathChanged);
    }
    let mut parent = folder;
    for (part, &id) in row.parts[..row.parts.len() - 1].iter().zip(&ids[1..]) {
        let next = open_dir(&parent, part).map_err(|_| Removal::PathChanged)?;
        if !fstat_is(&next, id) {
            return Err(Removal::PathChanged);
        }
        parent = next;
    }
    let build = open_dir(&parent, last).map_err(|_| Removal::PathChanged)?;
    if !fstat_is(&build, build_id) {
        return Err(Removal::PathChanged);
    }
    hook(Stage::Opened, &row.path);
    empty(&build, build_id.0, &row.path).map_err(Removal::Failed)?;
    drop(build);
    let holds = rustix::fs::statat(&parent, *last, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|stat| id_of(&stat) == build_id);
    if !holds {
        return Err(Removal::Failed(
            "its contents were removed, but its name no longer holds it: the empty directory \
             is left"
                .to_string(),
        ));
    }
    rustix::fs::unlinkat(&parent, *last, AtFlags::REMOVEDIR)
        .map_err(|error| Removal::Failed(format!("{}: {error}", row.path.display())))
}

/// Empty the directory `dir` (shown as `shown`) from its own fd: a file or
/// symlink unlinked, a directory opened through it (as the entry read),
/// emptied and removed; a transcript or an entry on another device than
/// `dev` stops it.
fn empty(dir: &OwnedFd, dev: u64, shown: &Path) -> Result<(), String> {
    let failed = |error: rustix::io::Errno| format!("{}: {error}", shown.display());
    let mut names = Vec::new();
    for entry in Dir::read_from(dir).map_err(failed)? {
        let name = entry.map_err(failed)?.file_name().to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            names.push(name);
        }
    }
    for name in names {
        let path = shown.join(OsStr::from_bytes(name.as_bytes()));
        if Path::new(OsStr::from_bytes(name.as_bytes()))
            .extension()
            .is_some_and(|ext| ext == "jsonl")
        {
            return Err(format!(
                "{} is a transcript: it is never removed",
                path.display()
            ));
        }
        if name.as_bytes() == b".git" {
            return Err(format!(
                "{} is a git repository's: it is never removed",
                path.display()
            ));
        }
        let failed = |error: rustix::io::Errno| format!("{}: {error}", path.display());
        let stat = match rustix::fs::statat(dir, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(failed(error)),
        };
        let id = id_of(&stat);
        if id.0 != dev {
            return Err(format!("{} is on another device", path.display()));
        }
        if FileType::from_raw_mode(stat.st_mode).is_dir() {
            let child = open_dir(dir, OsStr::from_bytes(name.as_bytes())).map_err(failed)?;
            if !fstat_is(&child, id) {
                return Err(format!("{} changed while it was removed", path.display()));
            }
            empty(&child, dev, &path)?;
            drop(child);
            rustix::fs::unlinkat(dir, name.as_c_str(), AtFlags::REMOVEDIR).map_err(failed)?;
        } else {
            rustix::fs::unlinkat(dir, name.as_c_str(), AtFlags::empty()).map_err(failed)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
