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
//! Every answer is JSON.

pub(crate) mod index;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;
use spawn_postgres::provenance::{ProvChangeKind, ProvChangeRecord, ProvEntryRecord};

use crate::Flags;

/// The usage of `spawn prov`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn prov index --source <source> [--repo <git dir>]\n       \
     (records the Git history of the source's main and of its spawn/* branches; safe to run \
     again)\n       \
     spawn prov who <path>[:<line>] --source <source> [--repo <git dir>]\n       \
     spawn prov show <change id|task|commit> [--source <source>] [--repo <git dir>]";

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
                    let full = index::Repo::open(&repo)?
                        .resolve(&word)
                        .ok_or(error)?;
                    Target::Commit(full)
                }
            };
            let store = crate::open_store()?;
            println!("{}", pretty(&show(&store, &target)?));
            Ok(0)
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
}

/// The target `word` names: a `change_id` (`c` and 26 base32 characters),
/// a task (digits), or a full commit id (40 hex characters, any case).
pub(crate) fn target(word: &str) -> Result<Target, String> {
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
        "{word:?} is no change id (c and 26 base32 characters), task number or full commit id"
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
        if let Some(change) = store.prov_change_by_id(id).map_err(|error| error.to_string())? {
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
    let change = change_for_commit(store, domain, &commit)?;
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
        "change": change.as_ref().map(change_json),
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
