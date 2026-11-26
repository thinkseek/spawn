//! Universal file provenance, the Git domain (PV2, docs/provenance.md):
//! `spawn prov index` fills `prov_changes`, `prov_entries` and `prov_edges`
//! from a source's history and the event log ([`index`]), and `who` and
//! `show` read them back. Two read-only routes answer the same reads
//! (`serve/prov.rs`): they read the database alone, so a line's blame,
//! which needs the repository, is the command's only.
//!
//! - `index --source <source> [--repo <git dir>]`: idempotent; a second run
//!   records nothing, and a run killed midway is completed by the next.
//!   The domain is `git:<source>`, the source as missions name it; the
//!   repository read is `--repo`, else the source itself.
//! - `who <path>[:<line>]`: who made the path as `main` holds it. With a
//!   line, `git blame` of `main` names the commit that made that line and
//!   the Change recorded for it (a checkpoint's `edit` revision, a
//!   landing's `land`, an `import`); without, the last first-parent commit
//!   of `main` that touched the path, its Change and the revision its
//!   content came from (the entry's `origin`).
//! - `show <change id|task|commit>`: every revision of a Change, every
//!   Change of a task, or the Change recorded for a commit (read through
//!   the commit map both ways), each with its entries and edges.
//!
//! PV3a adds:
//!
//! - the landing hook ([`record_land`]): right after a landing's push, the
//!   integrator records the landing's checkpoints and its `land` Change by
//!   the indexer's own code, so the rows are the ones `spawn prov index`
//!   writes. It never fails or delays a landing ([`record_land_logged`]).
//! - `blast <change|task|commit>` ([`blast`]): the later Changes on the
//!   same paths, the landings that include it, and the `used` closure.
//! - `revert --plan`, `split --plan --paths`, `compose --plan`
//!   ([`plan`]): the exact result tree and the commands; nothing written.
//!
//! PV3b adds the data domain ([`data`]): `snapshot`, `refs` and
//! `rollback [--plan]` for directories that are not in Git, stored in
//! spawn-cas; and `blast` follows `used` edges across data snapshots and
//! from code commits into them.
//!
//! Every answer is JSON.

pub(crate) mod data;
pub(crate) mod index;
#[cfg(test)]
mod land_tests;
pub(crate) mod plan;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;
use spawn_postgres::provenance::{
    ProvChangeKind, ProvChangeRecord, ProvDirection, ProvEdgeKind, ProvEntryClass, ProvEntryRecord,
    ProvLink,
};

use crate::Flags;

/// The usage of `spawn prov`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn prov index --source <source> [--repo <git dir>]\n       \
     (records the Git history of the source's main and of its spawn/* branches; safe to run \
     again)\n       \
     spawn prov who <path>[:<line>] --source <source> [--repo <git dir>]\n       \
     spawn prov show <change id|task|commit> [--source <source>] [--repo <git dir>]\n       \
     spawn prov blast <change id|task|commit> [--source <source>] [--repo <git dir>]\n       \
     spawn prov revert --plan <change id|task|commit> --source <source> [--repo <git dir>] \
     [--onto <commit>]\n       \
     spawn prov split --plan <change id|task|commit> --paths <path>[,<path>...] --source \
     <source> [--repo <git dir>]\n       \
     spawn prov compose --plan <change id|task|commit>... --source <source> [--repo <git dir>] \
     [--onto <commit>]\n       \
     (a plan prints the result tree and the commands and writes nothing: it is carried out as \
     a task's candidate, through the gate)\n       \
     spawn prov snapshot <dir> --ref <name> [--cas <dir>] [--input <ref|sha256:<hex>>]... \
     [--code <commit>]... [--config <file>]... [--task N] [--digest-only]\n       \
     (a data directory outside every workspace: its manifest-v1 into spawn-cas, one revision \
     of data:<name>; the 3 newest snapshots keep their bytes, every manifest stays)\n       \
     spawn prov refs [--cas <dir>]\n       \
     spawn prov rollback [--plan] <ref> <snapshot> [--cas <dir>] [--task N]\n       \
     (moves the ref's head back to a snapshot it still keeps, as a revert revision; --plan \
     writes nothing)";

/// `spawn prov <verb> ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some(verb) = rest.first() else {
        return Err(USAGE.to_string());
    };
    match verb.as_str() {
        "index" => {
            let flags = Flags::parse(&rest[1..])?;
            let source = flags.required("source")?;
            let repo = flags.get("repo").unwrap_or_else(|| source.clone());
            let store = crate::open_store()?;
            let mut git = index::Repo::open(&repo)?;
            let domain = index::domain_of(&source);
            let summary = index::index(&store, &mut git, &domain)?;
            println!(
                "{}",
                json!({
                    "domain": domain,
                    "recorded": summary.recorded(),
                    "edit": summary.edits,
                    "land": summary.lands,
                    "import": summary.imports,
                    "main_commits_recorded_before": summary.known,
                })
            );
            Ok(0)
        }
        "who" => {
            let (word, flags) = positional(&rest[1..])?;
            let source = flags.required("source")?;
            let repo = flags.get("repo").unwrap_or_else(|| source.clone());
            let store = crate::open_store()?;
            let git = index::Repo::open(&repo)?;
            let answer = who(&store, &git, &index::domain_of(&source), &word)?;
            println!("{}", pretty(&answer));
            Ok(0)
        }
        "show" => {
            let (word, flags) = positional(&rest[1..])?;
            let repo = flags.get("repo").or_else(|| flags.get("source"));
            let target = match target(&word) {
                Ok(target) => target,
                Err(error) => {
                    let Some(repo) = repo else {
                        return Err(error);
                    };
                    let full = index::Repo::open(&repo)?.resolve(&word).ok_or(error)?;
                    Target::Commit(full)
                }
            };
            let store = crate::open_store()?;
            println!("{}", pretty(&show(&store, &target)?));
            Ok(0)
        }
        "blast" => {
            let (word, flags) = positional(&rest[1..])?;
            let repo = flags.get("repo").or_else(|| flags.get("source"));
            let target = match target(&word) {
                Ok(target) => target,
                Err(error) => {
                    let Some(repo) = repo else {
                        return Err(error);
                    };
                    let full = index::Repo::open(&repo)?.resolve(&word).ok_or(error)?;
                    Target::Commit(full)
                }
            };
            let store = crate::open_store()?;
            println!("{}", pretty(&blast(&store, &target)?));
            Ok(0)
        }
        "revert" | "split" | "compose" => plan_command(verb, &rest[1..]),
        "snapshot" | "refs" | "rollback" => data_command(verb, &rest[1..]),
        _ => Err(USAGE.to_string()),
    }
}

/// `spawn prov snapshot|refs|rollback ...` (PV3b, [`data`]).
fn data_command(verb: &str, rest: &[String]) -> Result<i32, String> {
    match verb {
        "snapshot" => {
            let args = data::snapshot_args(rest)?;
            let store = crate::open_store()?;
            println!("{}", pretty(&data::snapshot(&store, &args)?));
            Ok(0)
        }
        "refs" => {
            let flags = Flags::parse(rest)?;
            let cas = match flags.get("cas") {
                Some(dir) => Some(spawn_cas::Store::open(dir).map_err(|error| error.to_string())?),
                None => None,
            };
            let store = crate::open_store()?;
            println!("{}", pretty(&data::refs(&store, cas.as_ref())?));
            Ok(0)
        }
        "rollback" => {
            let (words, planned, flags) = plan_arguments(rest)?;
            let [name, snapshot] = words.as_slice() else {
                return Err(USAGE.to_string());
            };
            let task = flags
                .get("task")
                .map(|task| task.parse::<i64>().map_err(|_| format!("bad task {task}")))
                .transpose()?;
            let cas = spawn_cas::Store::open(data::cas_dir(flags.get("cas").as_deref())?)
                .map_err(|error| error.to_string())?;
            let store = crate::open_store()?;
            let (code, answer) = data::rollback(&store, &cas, name, snapshot, planned, task)?;
            println!("{}", pretty(&answer));
            Ok(code)
        }
        _ => Err(USAGE.to_string()),
    }
}

/// The first word of `rest`, which is no flag, and the flags after it.
fn positional(rest: &[String]) -> Result<(String, Flags), String> {
    match rest.split_first() {
        Some((word, flags)) if !word.starts_with("--") => Ok((word.clone(), Flags::parse(flags)?)),
        _ => Err(USAGE.to_string()),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// What `show` and `/api/prov/change/<id>` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A Change, every revision of it.
    Change(String),
    /// Every Change of a task.
    Task(i64),
    /// The Change recorded for a commit (a full id).
    Commit(String),
    /// The Changes whose result is a snapshot (`sha256:<hex>`, a data
    /// snapshot).
    Snapshot(String),
}

/// The target `word` names: a `change_id` (`c` and 26 base32 characters),
/// a task (digits), a full commit id (40 hex characters, any case), a data
/// ref (`data:<ref>`, its Change) or a snapshot (`sha256:<hex>`; an
/// unprefixed digest is refused).
pub(crate) fn target(word: &str) -> Result<Target, String> {
    if let Some(name) = word.strip_prefix(data::DOMAIN_PREFIX) {
        data::check_ref(name)?;
        return Ok(Target::Change(data::change_id_of(name)));
    }
    if word.contains(':') {
        return data::parse_digest(word).map(|digest| Target::Snapshot(digest.to_string()));
    }
    if spawn_cas::is_hex64(&word.to_ascii_lowercase()) {
        return Err(data::parse_digest(word)
            .err()
            .unwrap_or_else(|| format!("{word}: an unprefixed digest")));
    }
    let base32 = |text: &str| {
        text.bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
    };
    if word.len() == 27 && word.starts_with('c') && base32(&word[1..]) {
        return Ok(Target::Change(word.to_string()));
    }
    if !word.is_empty() && word.len() <= 18 && word.bytes().all(|byte| byte.is_ascii_digit()) {
        return word
            .parse()
            .map(Target::Task)
            .map_err(|_| format!("bad task {word}"));
    }
    if word.len() == 40 && word.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(Target::Commit(word.to_ascii_lowercase()));
    }
    Err(format!(
        "{word:?} is no change id (c and 26 base32 characters), task number, full commit id, \
         data:<ref> or sha256:<hex> snapshot"
    ))
}

/// A recorded change as the answers show it.
pub(crate) fn change_json(change: &ProvChangeRecord) -> Value {
    json!({
        "id": change.id,
        "change_id": change.change_id,
        "rev": change.rev,
        "kind": change.kind.as_str(),
        "domain": change.domain,
        "agent_id": change.agent_id,
        "execution_id": change.execution_id,
        "task_id": change.task_id,
        "preset_id": change.preset_id,
        "model_label": change.model_label,
        "transcript_ref": change.transcript_ref,
        "attributed": change.attributed.as_str(),
        "base": change.base,
        "result": change.result,
        "boundary": change.boundary.as_str(),
        "at_ms": change.at_ms,
    })
}

/// A recorded entry as the answers show it: its origin also by the
/// `change_id` and `rev` it names (row ids differ from database to
/// database).
fn entry_json(entry: &ProvEntryRecord, origins: &HashMap<i64, ProvChangeRecord>) -> Value {
    let origin = entry.origin.and_then(|id| origins.get(&id));
    json!({
        "path": entry.path,
        "before": entry.before,
        "after": entry.after,
        "size": entry.size,
        "class": entry.class.as_str(),
        "origin": entry.origin,
        "origin_change_id": origin.map(|change| change.change_id.clone()),
        "origin_rev": origin.map(|change| change.rev),
    })
}

/// The changes `ids` names, by id (each read once).
fn changes_by_id(
    store: &PostgresStore,
    ids: impl IntoIterator<Item = i64>,
) -> Result<HashMap<i64, ProvChangeRecord>, String> {
    let mut found = HashMap::new();
    for id in ids {
        if found.contains_key(&id) {
            continue;
        }
        if let Some(change) = store
            .prov_change_by_id(id)
            .map_err(|error| error.to_string())?
        {
            found.insert(id, change);
        }
    }
    Ok(found)
}

/// The changes `target` names, oldest first.
pub(crate) fn changes_of(
    store: &PostgresStore,
    target: &Target,
) -> Result<Vec<ProvChangeRecord>, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    Ok(match target {
        Target::Change(change_id) => {
            let mut revisions = Vec::new();
            let mut rev = 1;
            while let Some(change) = store.prov_change(change_id, rev).map_err(error)? {
                revisions.push(change);
                rev += 1;
            }
            revisions
        }
        Target::Task(task) => store.prov_changes_for_task(*task).map_err(error)?,
        Target::Commit(commit) => store
            .prov_changes_for_result(&index::git1(commit))
            .map_err(error)?,
        Target::Snapshot(snapshot) => store.prov_changes_for_result(snapshot).map_err(error)?,
    })
}

/// `show`'s answer: `{changes: [{change, entries, edges}]}`.
pub(crate) fn show(store: &PostgresStore, target: &Target) -> Result<Value, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    let mut shown = Vec::new();
    for change in changes_of(store, target)? {
        let entries = store.prov_entries(change.id).map_err(error)?;
        let origins = changes_by_id(store, entries.iter().filter_map(|entry| entry.origin))?;
        let edges: Vec<Value> = store
            .prov_edges(change.id)
            .map_err(error)?
            .iter()
            .map(|edge| {
                json!({
                    "kind": edge.kind.as_str(),
                    "dst": edge.dst,
                    "detail": edge.detail,
                    "at_ms": edge.at_ms,
                })
            })
            .collect();
        shown.push(json!({
            "change": change_json(&change),
            "entries": entries.iter().map(|entry| entry_json(entry, &origins)).collect::<Vec<_>>(),
            "edges": edges,
        }));
    }
    Ok(json!({ "changes": shown }))
}

/// A `<path>[:<line>]` word: the path, and the line when the word ends in
/// `:<digits>` (from 1).
pub(crate) fn path_and_line(word: &str) -> Result<(String, Option<u32>), String> {
    if let Some((path, line)) = word.rsplit_once(':')
        && !line.is_empty()
        && line.bytes().all(|byte| byte.is_ascii_digit())
    {
        let line: u32 = line.parse().map_err(|_| format!("bad line {line}"))?;
        if line == 0 {
            return Err("lines count from 1".to_string());
        }
        return Ok((path.to_string(), Some(line)));
    }
    Ok((word.to_string(), None))
}

/// The top-level directory a path folds into in a `tree` entry, if it is
/// under one.
fn folded(path: &str) -> Option<&str> {
    path.split_once('/').map(|(top, _)| top)
}

/// The Change recorded in `domain` for `commit`, preferring a checkpoint's
/// or a landing's over any other.
fn change_for_commit(
    store: &PostgresStore,
    domain: &str,
    commit: &str,
) -> Result<Option<ProvChangeRecord>, String> {
    Ok(store
        .prov_changes_for_result(&index::git1(commit))
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|change| change.domain == domain))
}

/// The revision an entry's content came from: its `origin`, when set.
fn origin_of(
    store: &PostgresStore,
    change: &ProvChangeRecord,
    path: &str,
) -> Result<(Option<ProvEntryRecord>, Option<ProvChangeRecord>), String> {
    let history = store
        .prov_path_history(&change.domain, path, folded(path))
        .map_err(|error| error.to_string())?;
    let Some((_, entry)) = history.into_iter().find(|(found, _)| found.id == change.id) else {
        return Ok((None, None));
    };
    let origin = match entry.origin {
        Some(id) => store
            .prov_change_by_id(id)
            .map_err(|error| error.to_string())?,
        None => None,
    };
    Ok((Some(entry), origin))
}

/// `who`'s answer for `word` (`<path>[:<line>]`) in `domain`, from `repo`'s
/// `main` (module doc).
pub(crate) fn who(
    store: &PostgresStore,
    repo: &index::Repo,
    domain: &str,
    word: &str,
) -> Result<Value, String> {
    let (path, line) = path_and_line(word)?;
    let main = repo
        .resolve(index::MAIN)
        .ok_or_else(|| format!("the repository has no {}", index::MAIN))?;
    let commit = match line {
        Some(line) => {
            let range = format!("{line},{line}");
            let blame = repo.git(&["blame", "--porcelain", "-L", &range, &main, "--", &path])?;
            String::from_utf8_lossy(&blame)
                .split_whitespace()
                .next()
                .map(str::to_string)
                .ok_or_else(|| format!("git blame names no commit for {path}:{line}"))?
        }
        None => String::from_utf8_lossy(&repo.git(&[
            "log",
            "--first-parent",
            "-1",
            "--format=%H",
            &main,
            "--",
            &path,
        ])?)
        .trim()
        .to_string(),
    };
    if commit.is_empty() {
        return Err(format!("main never held {path}"));
    }
    let mut change = change_for_commit(store, domain, &commit)?;
    let mut via = change.as_ref().map(|_| "commit");
    // A commit no Change records (a landing squashed, so its second parent
    // is gone, or exported): its integrator's trailers name the landing's
    // Change (`Spawn-Task` and `Spawn-Head` give its key), and its
    // `Spawn-Change` lines the source Changes it included (D1).
    let message =
        String::from_utf8_lossy(&repo.git(&["log", "-1", "--format=%B", &commit, "--"])?)
            .into_owned();
    if change.is_none() {
        let trailers = index::trailers(&message);
        if let (Some(task), Some(head)) = (trailers.task, trailers.head) {
            let change_id = index::change_id(domain, &index::land_key(task, &head));
            change = changes_of(store, &Target::Change(change_id))?.pop();
            if change.is_some() {
                via = Some("trailers");
            }
        }
    }
    let (entry, origin) = match (&change, line) {
        (Some(change), None) => origin_of(store, change, &path)?,
        _ => (None, None),
    };
    let author = origin.as_ref().or(change.as_ref());
    Ok(json!({
        "path": path,
        "line": line,
        "granularity": if line.is_some() { "line" } else { "file" },
        "commit": commit,
        "indexed": change.is_some(),
        "via": via,
        "change": change.as_ref().map(change_json),
        "spawn_change": spawn_changes(&message),
        "entry": entry.as_ref().map(|entry| entry_json(entry, &HashMap::new())),
        "origin": origin.as_ref().map(change_json),
        "agent_id": author.map(|change| change.agent_id.clone()),
        "execution_id": author.and_then(|change| change.execution_id.clone()),
        "task_id": author.and_then(|change| change.task_id),
        "attributed": author.map(|change| change.attributed.as_str()),
    }))
}

/// `/api/prov/who`'s answer for `path` in `domain` (`None`: every domain),
/// from the database alone: the last `land` or `import` Change with an
/// entry for the path (or for a `tree` of its top-level directory), that
/// entry, and the revision its content came from; and the path's whole
/// history, oldest first.
pub(crate) fn who_recorded(
    store: &PostgresStore,
    domain: Option<&str>,
    path: &str,
) -> Result<Value, String> {
    let history = if let Some(domain) = domain {
        store
            .prov_path_history(domain, path, folded(path))
            .map_err(|error| error.to_string())?
    } else {
        let mut domains: Vec<String> = store
            .prov_changes_for_path(path)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|change| change.domain)
            .collect();
        domains.sort();
        domains.dedup();
        let mut all = Vec::new();
        for domain in domains {
            all.extend(
                store
                    .prov_path_history(&domain, path, folded(path))
                    .map_err(|error| error.to_string())?,
            );
        }
        all.sort_by_key(|(change, _)| (change.at_ms, change.id));
        all
    };
    let last = history
        .iter()
        .rev()
        .find(|(change, _)| matches!(change.kind, ProvChangeKind::Land | ProvChangeKind::Import));
    let origins = changes_by_id(store, history.iter().filter_map(|(_, entry)| entry.origin))?;
    let origin = last.and_then(|(_, entry)| entry.origin.and_then(|id| origins.get(&id)));
    let author = origin.or(last.map(|(change, _)| change));
    Ok(json!({
        "path": path,
        "granularity": "file",
        "indexed": last.is_some(),
        "change": last.map(|(change, _)| change_json(change)),
        "entry": last.map(|(_, entry)| entry_json(entry, &origins)),
        "origin": origin.map(change_json),
        "agent_id": author.map(|change| change.agent_id.clone()),
        "execution_id": author.and_then(|change| change.execution_id.clone()),
        "task_id": author.and_then(|change| change.task_id),
        "attributed": author.map(|change| change.attributed.as_str()),
        "history": history
            .iter()
            .map(|(change, entry)| json!({
                "change": change_json(change),
                "entry": entry_json(entry, &origins),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// The `change_id` of task `task`'s `edit` Change in the domain of `source`
/// (decisions B2): what a landing's `Spawn-Change` trailer names (D1).
pub(crate) fn source_change(source: &str, task: i64) -> String {
    index::change_id(&index::domain_of(source), &index::edit_task_key(task))
}

/// The `Spawn-Change` lines of a message's last paragraph, in order.
pub(crate) fn spawn_changes(message: &str) -> Vec<String> {
    let text = message.trim_end();
    let last = text.rsplit("\n\n").next().unwrap_or_default();
    if last.len() == text.len() {
        return Vec::new();
    }
    last.lines()
        .filter_map(|line| line.strip_prefix("Spawn-Change: "))
        .map(|id| id.trim().to_string())
        .filter(|id| matches!(target(id), Ok(Target::Change(_))))
        .collect()
}

#[cfg(test)]
thread_local! {
    /// A test's database made unreachable at the landing hook.
    pub(crate) static HOOK_UNREACHABLE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// What [`record_land`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landed {
    /// Recorded: the landing's checkpoints and its `land` Change.
    Recorded(index::Summary),
    /// The landing was recorded already.
    Already,
    /// The landing's first parent is in no Change of the domain: main's
    /// history is not indexed yet, and `spawn prov index` records it whole,
    /// in order (a hook that recorded the landing alone could give its
    /// Changes other revisions than the walk would).
    NotIndexed,
}

/// The landing hook (PV3a): record the landing `candidate` of `source`
/// (in `mirror`, which holds it) as `spawn prov index` would, right after
/// its push. The indexer's own walk, run on a [`plan::View`] of the mirror
/// whose only branch is `main` at the candidate: with every earlier commit
/// of main recorded, the walk's only new first-parent commit is the
/// candidate, so it records the checkpoints the landing brings in, then
/// the `land` Change with its `includes` edges and origins, with the
/// revisions, attribution and entries the indexer gives them, and nothing
/// else (the mirror's other branches, which it does not keep up to date,
/// are not walked).
pub(crate) fn record_land(
    store: &PostgresStore,
    mirror: &Path,
    source: &str,
    candidate: &str,
) -> Result<Landed, String> {
    #[cfg(test)]
    if HOOK_UNREACHABLE.with(std::cell::Cell::get) {
        return Err("the database is unreachable (a test made it so)".to_string());
    }
    let domain = index::domain_of(source);
    let mirror_path = mirror.to_string_lossy().to_string();
    if change_for_commit(store, &domain, candidate)?.is_some() {
        return Ok(Landed::Already);
    }
    let mut repo = index::Repo::open(&mirror_path)?;
    let commit = repo.commit(candidate)?;
    match commit.parents.first() {
        Some(first) if change_for_commit(store, &domain, first)?.is_some() => {}
        _ => return Ok(Landed::NotIndexed),
    }
    drop(repo);
    let view = plan::View::of(&mirror_path)?;
    view.git(&["update-ref", index::MAIN, candidate])?;
    let mut repo = index::Repo::open(&view.path())?;
    let summary = index::index(store, &mut repo, &domain)?;
    Ok(Landed::Recorded(summary))
}

/// [`record_land`] for the integrator's landing of task `task`: what it
/// did, or what went wrong, is logged and never returned. Not even a panic
/// leaves it: a landing is pushed and recorded before, and provenance never
/// fails it (`spawn prov index` records the same rows later).
pub(crate) fn record_land_logged(
    store: &PostgresStore,
    mirror: &Path,
    source: &str,
    task: i64,
    candidate: &str,
) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        record_land(store, mirror, source, candidate)
    }));
    match outcome {
        Ok(Ok(Landed::Recorded(summary))) => println!(
            "integrate: task {task}: provenance recorded {} change(s) for {candidate}",
            summary.recorded()
        ),
        Ok(Ok(Landed::Already)) => {}
        Ok(Ok(Landed::NotIndexed)) => println!(
            "integrate: task {task}: provenance not recorded: main's history is not indexed in \
             {} yet (spawn prov index records it)",
            index::domain_of(source)
        ),
        Ok(Err(error)) => eprintln!(
            "integrate: task {task}: provenance not recorded (spawn prov index records it \
             later): {error}"
        ),
        Err(_) => eprintln!(
            "integrate: task {task}: provenance not recorded (the hook panicked; spawn prov \
             index records it later)"
        ),
    }
}

/// How far [`blast`] walks the dependents of a Change.
const BLAST_DEPTH: u16 = 64;

fn link_name(link: ProvLink) -> &'static str {
    match link {
        ProvLink::Edge(kind) => kind.as_str(),
        ProvLink::Origin => "origin",
    }
}

/// `blast`'s answer for `target`, from the database alone:
///
/// - `changes`: the Changes it names;
/// - `later`: every later Change (by time, then id) with an entry on a
///   path one of them touched, each with those paths; a landing that
///   includes one of them is in `landed_by` instead;
/// - `radius`: every Change that depends on them, by an edge or an entry's
///   `origin`, nearest first, each with its link and one shortest path;
/// - `used`: the part of the radius reached by `used` edges alone;
/// - `snapshots`: the data snapshots among the Changes and their radius
///   (PV3b), each with its ref, nearest first.
///
/// A commit or a snapshot that no recorded Change is is followed through
/// the edges that name it directly (`--code <commit>`, `--input
/// sha256:<hex>`), so a code commit reaches the data snapshots made with
/// it even when no Git domain records it.
pub(crate) fn blast(store: &PostgresStore, target: &Target) -> Result<Value, String> {
    let start = changes_of(store, target)?;
    let (after, landed_by) = later_on_paths(store, &start)?;
    let named = match target {
        Target::Commit(commit) => Some(index::git1(commit)),
        Target::Snapshot(snapshot) => Some(snapshot.clone()),
        Target::Change(_) | Target::Task(_) => None,
    };
    // Only when no recorded Change is the target: a Change's own walk
    // already follows the edges to its result (a Git commit's `land`, a
    // snapshot's revisions), so this would walk them twice.
    let users = match &named {
        Some(digest) if start.is_empty() => store
            .prov_changes_using(digest)
            .map_err(|error| error.to_string())?,
        _ => Vec::new(),
    };
    let (radius, used) = dependents(store, &start, &users, named.as_deref().unwrap_or_default())?;
    let mut order: Vec<(i64, i64, &'static str)> = start
        .iter()
        .map(|change| (change.id, 0, "target"))
        .collect();
    let mut reached: Vec<(i64, i64, &'static str)> = radius
        .iter()
        .map(|(id, value)| {
            let via = match value["via"].as_str() {
                Some("used") => "used",
                Some("includes") => "includes",
                Some("supersedes") => "supersedes",
                Some("redacts") => "redacts",
                _ => "origin",
            };
            (*id, value["depth"].as_i64().unwrap_or_default(), via)
        })
        .collect();
    reached.sort_by_key(|(id, depth, _)| (*depth, *id));
    order.extend(reached);
    let snapshots = data::snapshot_rows(
        &changes_by_id(store, order.iter().map(|(id, ..)| *id))?,
        &order,
    );
    let by_depth = |map: BTreeMap<i64, Value>| {
        let mut values: Vec<(i64, i64, Value)> = map
            .into_iter()
            .map(|(id, value)| (value["depth"].as_i64().unwrap_or_default(), id, value))
            .collect();
        values.sort_by_key(|(depth, id, _)| (*depth, *id));
        values
            .into_iter()
            .map(|(_, _, value)| value)
            .collect::<Vec<_>>()
    };
    Ok(json!({
        "indexed": !start.is_empty(),
        "changes": start.iter().map(change_json).collect::<Vec<_>>(),
        "later": after,
        "landed_by": landed_by,
        "radius": by_depth(radius),
        "used": by_depth(used),
        "snapshots": snapshots,
    }))
}

/// [`blast`]'s `later` and `landed_by`: the later Changes on the paths of
/// `start`, each with those paths, oldest first.
fn later_on_paths(
    store: &PostgresStore,
    start: &[ProvChangeRecord],
) -> Result<(Vec<Value>, Vec<Value>), String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    let ids: HashSet<i64> = start.iter().map(|change| change.id).collect();
    let change_ids: HashSet<&str> = start
        .iter()
        .map(|change| change.change_id.as_str())
        .collect();
    // Each path once per domain, from the earliest start Change touching it.
    let mut paths: BTreeMap<(String, String), (i64, i64, bool)> = BTreeMap::new();
    for change in start {
        for entry in store.prov_entries(change.id).map_err(error)? {
            let tree = entry.class == ProvEntryClass::Tree;
            let key = (change.domain.clone(), entry.path);
            let at = (change.at_ms, change.id, tree);
            paths
                .entry(key)
                .and_modify(|seen| {
                    if (at.0, at.1) < (seen.0, seen.1) {
                        *seen = at;
                    }
                })
                .or_insert(at);
        }
    }
    let mut later: BTreeMap<(i64, i64), (ProvChangeRecord, Vec<String>)> = BTreeMap::new();
    for ((domain, path), (at_ms, id, tree)) in &paths {
        let folded = if *tree { None } else { folded(path) };
        for (other, _) in store
            .prov_path_history(domain, path, folded)
            .map_err(error)?
        {
            if ids.contains(&other.id) || (other.at_ms, other.id) <= (*at_ms, *id) {
                continue;
            }
            let row = later
                .entry((other.at_ms, other.id))
                .or_insert_with(|| (other.clone(), Vec::new()));
            if !row.1.contains(path) {
                row.1.push(path.clone());
            }
        }
    }
    let mut landed_by = Vec::new();
    let mut after = Vec::new();
    for (change, paths) in later.into_values() {
        let shown = json!({"change": change_json(&change), "paths": paths});
        let lands_it = change.kind == ProvChangeKind::Land
            && store
                .prov_edges(change.id)
                .map_err(error)?
                .iter()
                .any(|edge| {
                    edge.kind == ProvEdgeKind::Includes && change_ids.contains(edge.dst.as_str())
                });
        if lands_it {
            landed_by.push(shown);
        } else {
            after.push(shown);
        }
    }
    Ok((after, landed_by))
}

/// The radius [`dependents`] builds: each Change once, at its least depth.
#[derive(Default)]
struct Radius {
    /// The start Changes, never in their own radius.
    start: HashSet<i64>,
    nodes: BTreeMap<i64, Value>,
    depths: HashMap<i64, i64>,
    used: BTreeMap<i64, Value>,
}

impl Radius {
    fn add(&mut self, id: i64, depth: i64, shown: Value, by_used: bool) {
        if self.start.contains(&id) || self.depths.get(&id).is_some_and(|d| *d <= depth) {
            return;
        }
        self.depths.insert(id, depth);
        if by_used {
            self.used.insert(id, shown.clone());
        } else {
            self.used.remove(&id);
        }
        self.nodes.insert(id, shown);
    }
}

/// [`blast`]'s `radius` and `used`, by id: every Change that depends on
/// one of `start`, and every Change of `users` (they name `label`, a commit
/// or a snapshot, by an edge) at depth 1 with everything that depends on
/// it.
#[allow(clippy::type_complexity)]
fn dependents(
    store: &PostgresStore,
    start: &[ProvChangeRecord],
    users: &[(ProvChangeRecord, ProvEdgeKind)],
    label: &str,
) -> Result<(BTreeMap<i64, Value>, BTreeMap<i64, Value>), String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    let mut names: HashMap<i64, String> = start
        .iter()
        .map(|change| (change.id, format!("{}#{}", change.change_id, change.rev)))
        .collect();
    let mut found = Radius {
        start: start.iter().map(|change| change.id).collect(),
        ..Radius::default()
    };
    // Each walk: from a Change, at a depth, after a path, used all along.
    let mut walks: Vec<(&ProvChangeRecord, i64, Vec<String>, bool)> = start
        .iter()
        .map(|change| (change, 0, Vec::new(), true))
        .collect();
    for (user, kind) in users {
        let name = format!("{}#{}", user.change_id, user.rev);
        names.insert(user.id, name.clone());
        found.add(
            user.id,
            1,
            json!({
                "change_id": user.change_id,
                "rev": user.rev,
                "agent_id": user.agent_id,
                "depth": 1,
                "via": kind.as_str(),
                "path": [label, name],
            }),
            *kind == ProvEdgeKind::Used,
        );
        walks.push((
            user,
            1,
            vec![label.to_string()],
            *kind == ProvEdgeKind::Used,
        ));
    }
    for (change, offset, prefix, prefix_used) in walks {
        let nodes = store
            .prov_blast_radius(change.id, ProvDirection::Forward, BLAST_DEPTH)
            .map_err(error)?;
        let links: HashMap<i64, ProvLink> = nodes.iter().map(|node| (node.id, node.via)).collect();
        for node in &nodes {
            names.insert(node.id, format!("{}#{}", node.change_id, node.rev));
        }
        for node in nodes {
            let depth = i64::from(node.depth) + offset;
            let mut path = prefix.clone();
            path.extend(
                node.path
                    .iter()
                    .map(|id| names.get(id).cloned().unwrap_or_else(|| id.to_string())),
            );
            let shown = json!({
                "change_id": node.change_id,
                "rev": node.rev,
                "agent_id": node.agent_id,
                "depth": depth,
                "via": link_name(node.via),
                "path": path,
            });
            let by_used = prefix_used
                && node
                    .path
                    .iter()
                    .skip(1)
                    .all(|id| links.get(id) == Some(&ProvLink::Edge(ProvEdgeKind::Used)));
            found.add(node.id, depth, shown, by_used);
        }
    }
    Ok((found.nodes, found.used))
}

/// The arguments of a plan: its words, whether `--plan` was given, and
/// its other flags (each with a value).
fn plan_arguments(rest: &[String]) -> Result<(Vec<String>, bool, Flags), String> {
    let mut words = Vec::new();
    let mut plan = false;
    let mut pairs = Vec::new();
    let mut rest = rest.iter();
    while let Some(word) = rest.next() {
        if word == "--plan" {
            plan = true;
        } else if word.starts_with("--") {
            pairs.push(word.clone());
            pairs.push(
                rest.next()
                    .ok_or_else(|| format!("{word} needs a value"))?
                    .clone(),
            );
        } else {
            words.push(word.clone());
        }
    }
    Ok((words, plan, Flags::parse(&pairs)?))
}

/// The one revision a plan applies for `target` in `domain`: a commit's
/// Change; a task's last `land`; a Change's last revision, or, for an
/// `edit` Change, the last landing that includes it.
fn planned_change(
    store: &PostgresStore,
    domain: &str,
    target: &Target,
) -> Result<ProvChangeRecord, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    let in_domain = |change: &ProvChangeRecord| change.domain == domain;
    match target {
        Target::Snapshot(snapshot) => Err(format!(
            "{snapshot} is a data snapshot: a Git plan applies commits (spawn prov rollback \
             --plan <ref> <snapshot> plans a data ref's)"
        )),
        Target::Commit(commit) => change_for_commit(store, domain, commit)?.ok_or_else(|| {
            format!("{commit} has no Change in {domain} (spawn prov index records it)")
        }),
        Target::Task(task) => changes_of(store, target)?
            .into_iter()
            .rfind(|change| in_domain(change) && change.kind == ProvChangeKind::Land)
            .ok_or_else(|| format!("task {task} has no landing in {domain}")),
        Target::Change(change_id) => {
            let last = changes_of(store, target)?
                .into_iter()
                .rfind(in_domain)
                .ok_or_else(|| format!("{change_id} is no Change of {domain}"))?;
            if last.kind != ProvChangeKind::Edit {
                return Ok(last);
            }
            let mut landings = Vec::new();
            for node in store
                .prov_blast_radius(last.id, ProvDirection::Forward, 1)
                .map_err(error)?
            {
                if node.via != ProvLink::Edge(ProvEdgeKind::Includes) {
                    continue;
                }
                if let Some(change) = store.prov_change_by_id(node.id).map_err(error)?
                    && change.kind == ProvChangeKind::Land
                    && in_domain(&change)
                {
                    landings.push(change);
                }
            }
            landings.sort_by_key(|change| (change.at_ms, change.id));
            landings.pop().ok_or_else(|| {
                format!(
                    "{change_id} has not landed: name one of its commits to plan on a \
                     checkpoint"
                )
            })
        }
    }
}

/// `change` as a plan applies it, from `repo`.
fn pick_of(repo: &mut index::Repo, change: &ProvChangeRecord) -> Result<plan::Pick, String> {
    let recorded = change
        .result
        .strip_prefix("git1:")
        .ok_or_else(|| format!("{} is no git commit", change.result))?;
    let result = repo
        .resolve(recorded)
        .ok_or_else(|| format!("the repository does not hold {recorded}"))?;
    let commit = repo.commit(&result)?;
    let base = commit
        .parents
        .first()
        .cloned()
        .ok_or_else(|| format!("{result} is a root commit: there is no plan for it"))?;
    Ok(plan::Pick {
        label: format!("{}#{}", change.change_id, change.rev),
        base,
        result,
        merge: commit.parents.len() > 1,
    })
}

/// `spawn prov revert|split|compose --plan ...`.
fn plan_command(verb: &str, rest: &[String]) -> Result<i32, String> {
    let (words, planned, flags) = plan_arguments(rest)?;
    if !planned {
        return Err(format!(
            "only `spawn prov {verb} --plan` is built: a {verb} is carried out as an ordinary \
             candidate branch, through the gate (decisions B4)\n{USAGE}"
        ));
    }
    match (verb, words.len()) {
        ("compose", 0) => return Err(USAGE.to_string()),
        ("compose", _) | (_, 1) => {}
        _ => return Err(USAGE.to_string()),
    }
    let source = flags.required("source")?;
    let repo_path = flags.get("repo").unwrap_or_else(|| source.clone());
    let domain = index::domain_of(&source);
    let mut repo = index::Repo::open(&repo_path)?;
    let store = crate::open_store()?;
    let mut picks = Vec::new();
    for word in &words {
        let target = match target(word) {
            Ok(target) => target,
            Err(error) => Target::Commit(repo.resolve(word).ok_or(error)?),
        };
        let change = planned_change(&store, &domain, &target)?;
        picks.push(pick_of(&mut repo, &change)?);
    }
    let onto = match flags.get("onto") {
        Some(onto) => repo
            .resolve(&onto)
            .ok_or_else(|| format!("{onto} names no commit"))?,
        None => repo
            .resolve(index::MAIN)
            .ok_or_else(|| format!("the repository has no {}", index::MAIN))?,
    };
    let view = plan::View::of(&repo_path)?;
    let wanted = flags
        .get("paths")
        .map(|paths| plan::wanted_paths(&paths))
        .unwrap_or_default();
    for wanted in &wanted {
        if let plan::Wanted::Part { path, lines } = wanted {
            println!(
                "{}",
                pretty(&plan::refusal(&view, verb, &picks[0], path, lines))
            );
            return Ok(1);
        }
    }
    let answer = match verb {
        "revert" => {
            if !wanted.is_empty() {
                return Err("revert takes no --paths: split the Change first".to_string());
            }
            plan::revert(&view, &onto, &picks[0])?
        }
        "split" => {
            let paths: Vec<String> = wanted
                .into_iter()
                .filter_map(|wanted| match wanted {
                    plan::Wanted::Whole(path) => Some(path),
                    plan::Wanted::Part { .. } => None,
                })
                .collect();
            if paths.is_empty() {
                return Err("split needs --paths <path>[,<path>...]".to_string());
            }
            plan::split(&view, &picks[0], &paths)?
        }
        _ => {
            if !wanted.is_empty() {
                return Err(
                    "compose takes whole Changes: split one first (spawn prov split --plan \
                     --paths)"
                        .to_string(),
                );
            }
            plan::compose(&view, &onto, &picks)?
        }
    };
    println!("{}", pretty(&answer));
    Ok(0)
}
