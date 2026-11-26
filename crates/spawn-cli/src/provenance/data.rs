//! Universal file provenance, the data domain (PV3b, docs/provenance.md):
//! directories that are not in Git (datasets, shards, model outputs),
//! snapshotted into spawn-cas and recorded in the same three tables.
//!
//! - `snapshot <dir> --ref <name>`: the directory's manifest-v1
//!   ([`spawn_cas::Manifest`]), its objects stored in the `data` namespace
//!   (the manifest alone with `--digest-only`), and one revision of the
//!   ref's Change: `derive` with its `used` edges (`--input`, `--code`,
//!   `--config`), `import` when nothing is an input. The entries are the
//!   paths that differ from the ref's head. A directory equal to the head
//!   records nothing. `pins/prov/<ref>` keeps the three newest snapshots.
//! - `refs`: every data ref and its head.
//! - `rollback [--plan] <ref> <snapshot>`: the ref's head moved back to a
//!   snapshot it held, recorded as a `revert` revision; refused when the
//!   snapshot's bytes are no longer kept. `--plan` writes nothing.
//!
//! Identity (decisions B2, D2): the domain is `data:<ref>`, the Change's
//! key `data:<ref>` ([`change_id_of`]), every revision `max(rev) + 1`; a
//! snapshot is `sha256:<tree_sha256_v1>`, and an unprefixed digest is
//! refused everywhere. The head of a ref is the newest revision of its
//! domain; a result equal to the head is never recorded again (the
//! idempotency of `(change_id, result)`), so a run killed at any instant is
//! completed by running it again.
//!
//! What a data root may be (D5): never inside a workspace root
//! (`SPAWN_WORKSPACE_ROOT`, or any directory above it holding the
//! transcript archive `.transcripts`), never holding a transcript directory
//! (so no `*.jsonl` transcript is ever copied into the store), inside
//! `SPAWN_DATA_ROOT` when that is set, and with no symbolic link that leaves
//! it (links are never followed).

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use spawn_cas::{CasError, Digest, KEEP_SNAPSHOTS, Manifest, Store};
use spawn_postgres::PostgresStore;
use spawn_postgres::provenance::{
    ProvAttributionSource, ProvBoundary, ProvChangeInput, ProvChangeKind, ProvChangeRecord,
    ProvEdgeInput, ProvEdgeKind, ProvEntryClass, ProvEntryInput,
};

use super::index;

/// Where a data ref's domain name starts.
pub(crate) const DOMAIN_PREFIX: &str = "data:";

/// The directory names of the transcript archive and its records, which a
/// data root never holds (`spawn_host::transcripts`).
const TRANSCRIPT_DIRECTORIES: [&str; 3] = [
    ".transcripts",
    ".transcripts-progress",
    ".transcripts-sources",
];

/// The CAS under the workspace root when `--cas` is not given.
const DEFAULT_CAS: &str = ".cas";

/// The domain of the data ref `name`.
pub(crate) fn domain_of(name: &str) -> String {
    format!("{DOMAIN_PREFIX}{name}")
}

/// The `change_id` of the data ref `name` (B2): domain and key both
/// `data:<ref>`.
pub(crate) fn change_id_of(name: &str) -> String {
    let domain = domain_of(name);
    index::change_id(&domain, &domain)
}

/// A data ref's name: `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}` (a pin name with
/// no `:`, so a ref is never read as a digest), and never 64 hex digits
/// (an unprefixed digest).
pub(crate) fn check_ref(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    let good = bytes
        .first()
        .is_some_and(|first| first.is_ascii_alphanumeric() || *first == b'_')
        && bytes.len() <= 128
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte));
    if !good {
        return Err(format!(
            "data ref {name:?}: not [A-Za-z0-9_][A-Za-z0-9._-]{{0,127}}"
        ));
    }
    if spawn_cas::is_hex64(&name.to_ascii_lowercase()) {
        return Err(format!(
            "{name:?} is an unprefixed digest, refused: name a ref, or write sha256:<hex>"
        ));
    }
    Ok(())
}

/// An algorithm-prefixed digest; an unprefixed one is refused with a
/// plain message.
pub(crate) fn parse_digest(word: &str) -> Result<Digest, String> {
    if spawn_cas::is_hex64(&word.to_ascii_lowercase()) {
        return Err(format!(
            "{word} is an unprefixed digest, refused: write sha256:{}",
            word.to_ascii_lowercase()
        ));
    }
    Digest::parse(word).map_err(|error| error.to_string())
}

/// The empty snapshot: the base of a ref's first revision.
fn empty_snapshot() -> Digest {
    Manifest::default().digest()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The store: `--cas`, else `<SPAWN_WORKSPACE_ROOT>/.cas`.
pub(crate) fn cas_dir(given: Option<&str>) -> Result<PathBuf, String> {
    match given {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => env_path("SPAWN_WORKSPACE_ROOT")
            .map(|root| root.join(DEFAULT_CAS))
            .ok_or_else(|| "--cas <dir> is required (SPAWN_WORKSPACE_ROOT is not set)".to_string()),
    }
}

/// `path` made absolute and resolved as far as it exists (a missing tail
/// kept as written).
fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let absolute =
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |here| here.join(path));
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) if parent != absolute => resolved(parent).join(name),
        _ => absolute,
    }
}

fn is_transcript_name(name: &OsStr) -> bool {
    TRANSCRIPT_DIRECTORIES
        .iter()
        .any(|directory| name.as_bytes() == directory.as_bytes())
}

/// The data root `dir`, resolved, or why it may not be one (D5, module
/// doc); `cas` may not be inside it either.
pub(crate) fn check_root(dir: &Path, cas: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(dir)
        .map_err(|error| format!("data root {}: {error}", dir.display()))?;
    if !root.is_dir() {
        return Err(format!("data root {} is not a directory", root.display()));
    }
    let refuse = |why: String| Err(format!("refused: data root {}: {why}", root.display()));
    if root
        .components()
        .any(|part| is_transcript_name(part.as_os_str()))
    {
        return refuse("it is inside a transcript archive".to_string());
    }
    if let Some(workspace) = env_path("SPAWN_WORKSPACE_ROOT") {
        let workspace = resolved(&workspace);
        if root.starts_with(&workspace) {
            return refuse(format!(
                "it is inside the workspace root {} (SPAWN_WORKSPACE_ROOT): keep data outside \
                 every workspace",
                workspace.display()
            ));
        }
        if workspace.starts_with(&root) {
            return refuse(format!(
                "it holds the workspace root {} (SPAWN_WORKSPACE_ROOT)",
                workspace.display()
            ));
        }
    }
    if let Some(data_root) = env_path("SPAWN_DATA_ROOT") {
        let data_root = resolved(&data_root);
        if !root.starts_with(&data_root) {
            return refuse(format!(
                "it is outside SPAWN_DATA_ROOT ({})",
                data_root.display()
            ));
        }
    }
    for above in root.ancestors() {
        for name in TRANSCRIPT_DIRECTORIES {
            if std::fs::symlink_metadata(above.join(name)).is_ok() {
                return if above == root {
                    refuse(format!(
                        "it holds a transcript directory ({name}): transcripts are never \
                         snapshotted"
                    ))
                } else {
                    refuse(format!(
                        "it is inside a workspace root ({} holds {name})",
                        above.display()
                    ))
                };
            }
        }
    }
    if resolved(cas).starts_with(&root) {
        return refuse(format!(
            "the store {} is inside it",
            resolved(cas).display()
        ));
    }
    Ok(root)
}

/// Refuse a manifest that holds a transcript directory anywhere below its
/// root.
fn check_no_transcripts(root: &Path, manifest: &Manifest) -> Result<(), String> {
    for entry in manifest.entries() {
        let mut parts = entry.path.split(|&byte| byte == b'/');
        parts.next_back();
        if let Some(part) = parts.find(|part| is_transcript_name(OsStr::from_bytes(part))) {
            return Err(format!(
                "refused: data root {}: it holds a transcript directory ({}): transcripts are \
                 never snapshotted",
                root.display(),
                String::from_utf8_lossy(part)
            ));
        }
    }
    Ok(())
}

/// One path that differs between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diff {
    pub(crate) path: Vec<u8>,
    pub(crate) before: Option<Digest>,
    pub(crate) after: Option<Digest>,
    pub(crate) size: Option<u64>,
}

/// The paths where `after` differs from `before`, sorted bytewise.
pub(crate) fn diff(before: &Manifest, after: &Manifest) -> Vec<Diff> {
    let mut out = Vec::new();
    let (mut left, mut right) = (
        before.entries().iter().peekable(),
        after.entries().iter().peekable(),
    );
    loop {
        let order = match (left.peek(), right.peek()) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(old), Some(new)) => old.path.cmp(&new.path),
        };
        match order {
            std::cmp::Ordering::Less => {
                if let Some(old) = left.next() {
                    out.push(Diff {
                        path: old.path.clone(),
                        before: Some(old.digest.clone()),
                        after: None,
                        size: None,
                    });
                }
            }
            std::cmp::Ordering::Greater => {
                if let Some(new) = right.next() {
                    out.push(Diff {
                        path: new.path.clone(),
                        before: None,
                        after: Some(new.digest.clone()),
                        size: new.size,
                    });
                }
            }
            std::cmp::Ordering::Equal => {
                if let (Some(old), Some(new)) = (left.next(), right.next())
                    && old.digest != new.digest
                {
                    out.push(Diff {
                        path: new.path.clone(),
                        before: Some(old.digest.clone()),
                        after: Some(new.digest.clone()),
                        size: new.size,
                    });
                }
            }
        }
    }
    out
}

/// The entries of a Change from `before` to `after` (B2): one per path
/// that differs, or, past [`index::FOLD_OVER`], a path at the top as its
/// own row and one `tree` row per top-level directory, its digests that
/// directory's manifests (absent: no file under it). `origin` goes on
/// every entry.
pub(crate) fn entries(
    before: &Manifest,
    after: &Manifest,
    origin: Option<i64>,
) -> Vec<ProvEntryInput> {
    let diffs = diff(before, after);
    let size = |size: Option<u64>| size.and_then(|size| i64::try_from(size).ok());
    if diffs.len() <= index::FOLD_OVER {
        return diffs
            .into_iter()
            .map(|diff| ProvEntryInput {
                path: index::stored_path(&diff.path),
                before: diff.before.map(|digest| digest.to_string()),
                after: diff.after.map(|digest| digest.to_string()),
                size: size(diff.size),
                class: ProvEntryClass::Content,
                origin,
            })
            .collect();
    }
    let mut out = Vec::new();
    let mut directories: BTreeSet<Vec<u8>> = BTreeSet::new();
    for diff in diffs {
        match diff.path.iter().position(|&byte| byte == b'/') {
            Some(at) => {
                directories.insert(diff.path[..at].to_vec());
            }
            None => out.push(ProvEntryInput {
                path: index::stored_path(&diff.path),
                before: diff.before.map(|digest| digest.to_string()),
                after: diff.after.map(|digest| digest.to_string()),
                size: size(diff.size),
                class: ProvEntryClass::Content,
                origin,
            }),
        }
    }
    for directory in directories {
        let tree = |manifest: &Manifest| {
            let subtree = manifest.subtree(&directory);
            (!subtree.is_empty()).then(|| subtree.digest().to_string())
        };
        out.push(ProvEntryInput {
            path: index::stored_path(&directory),
            before: tree(before),
            after: tree(after),
            size: None,
            class: ProvEntryClass::Tree,
            origin,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The arguments of `spawn prov snapshot`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotArgs {
    pub(crate) dir: String,
    pub(crate) name: String,
    pub(crate) cas: Option<String>,
    pub(crate) inputs: Vec<String>,
    pub(crate) code: Vec<String>,
    pub(crate) config: Vec<String>,
    pub(crate) task: Option<i64>,
    pub(crate) digest_only: bool,
}

/// Parse `snapshot`'s words (after the verb): one directory, `--ref`
/// once, `--input`, `--code` and `--config` any number of times.
pub(crate) fn snapshot_args(rest: &[String]) -> Result<SnapshotArgs, String> {
    let mut args = SnapshotArgs::default();
    let mut dir = None;
    let mut name = None;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if word == "--digest-only" {
            args.digest_only = true;
            continue;
        }
        let Some(flag) = word.strip_prefix("--") else {
            if dir.replace(word.clone()).is_some() {
                return Err(format!("snapshot takes one directory, found {word} too"));
            }
            continue;
        };
        let value = words
            .next()
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("--{flag} needs a value"))?
            .clone();
        match flag {
            "ref" => {
                if name.replace(value).is_some() {
                    return Err("--ref is given once".to_string());
                }
            }
            "cas" => args.cas = Some(value),
            "input" => args.inputs.push(value),
            "code" => args.code.push(value),
            "config" => args.config.push(value),
            "task" => {
                args.task = Some(value.parse().map_err(|_| format!("bad task {value}"))?);
            }
            _ => return Err(format!("unknown flag --{flag}\n{}", super::USAGE)),
        }
    }
    args.dir = dir.ok_or_else(|| format!("snapshot needs a directory\n{}", super::USAGE))?;
    args.name = name.ok_or_else(|| "--ref <name> is required".to_string())?;
    Ok(args)
}

/// What a recorded Change is attributed to: a task's agent and preset
/// (`--task`, inferred: the flag is the caller's word), else `imported`
/// for an import and `unknown` otherwise.
struct Attribution {
    agent_id: String,
    task_id: Option<i64>,
    preset_id: Option<String>,
    model_label: Option<String>,
    attributed: ProvAttributionSource,
}

fn attribution(
    store: &PostgresStore,
    task: Option<i64>,
    kind: ProvChangeKind,
) -> Result<Attribution, String> {
    if let Some(task) = task {
        let facts = store
            .prov_task_facts(&[task])
            .map_err(|error| error.to_string())?;
        let facts = facts
            .get(&task)
            .ok_or_else(|| format!("task {task} is unknown"))?;
        return Ok(Attribution {
            agent_id: facts.agent_id.clone(),
            task_id: Some(task),
            preset_id: facts.preset_id.clone(),
            model_label: facts.model_label.clone(),
            attributed: ProvAttributionSource::Inferred,
        });
    }
    Ok(if kind == ProvChangeKind::Import {
        Attribution {
            agent_id: index::IMPORTED_AGENT.to_string(),
            task_id: None,
            preset_id: None,
            model_label: None,
            attributed: ProvAttributionSource::Imported,
        }
    } else {
        Attribution {
            agent_id: index::UNKNOWN_AGENT.to_string(),
            task_id: None,
            preset_id: None,
            model_label: None,
            attributed: ProvAttributionSource::Inferred,
        }
    })
}

/// The `used` edges of a snapshot: one per `--input` (a ref: its head
/// snapshot; a digest: itself), `--code` (`git1:<commit>`) and `--config`
/// (the file's SHA-256), each once.
fn used_edges(store: &PostgresStore, args: &SnapshotArgs) -> Result<Vec<ProvEdgeInput>, String> {
    let mut edges: Vec<ProvEdgeInput> = Vec::new();
    let mut push = |dst: String, detail: Value| {
        if !edges.iter().any(|edge| edge.dst == dst) {
            edges.push(ProvEdgeInput {
                kind: ProvEdgeKind::Used,
                dst,
                detail,
                at_ms: None,
            });
        }
    };
    for input in &args.inputs {
        if input.contains(':') {
            let digest = parse_digest(input)?;
            push(digest.to_string(), json!({"flag": "input"}));
            continue;
        }
        check_ref(input)?;
        let head = store
            .prov_head(&domain_of(input))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("--input {input}: data:{input} has no snapshot"))?;
        push(
            head.result.clone(),
            json!({"flag": "input", "ref": input, "change_id": head.change_id, "rev": head.rev}),
        );
    }
    for code in &args.code {
        let commit = code
            .strip_prefix("git1:")
            .unwrap_or(code)
            .to_ascii_lowercase();
        let hex = commit.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !hex || !(commit.len() == 40 || commit.len() == 64) {
            return Err(format!("--code {code}: not a full commit id"));
        }
        push(index::git1(&commit), json!({"flag": "code"}));
    }
    for config in &args.config {
        let bytes = std::fs::read(config).map_err(|error| format!("--config {config}: {error}"))?;
        push(
            spawn_cas::sha256_of(&bytes).to_string(),
            json!({"flag": "config", "path": resolved(Path::new(config)).display().to_string()}),
        );
    }
    Ok(edges)
}

/// The snapshots of a domain's changes, newest first, each once.
fn newest_results(changes: &[ProvChangeRecord]) -> Vec<Digest> {
    let mut out: Vec<Digest> = Vec::new();
    for change in changes {
        if let Ok(digest) = Digest::parse(&change.result)
            && !out.contains(&digest)
        {
            out.push(digest);
        }
    }
    out
}

/// The manifest of `snapshot` (the empty snapshot needs no object), or a
/// plain message when the store does not hold it.
fn manifest_of(cas: &Store, name: &str, snapshot: &Digest) -> Result<Manifest, String> {
    if *snapshot == empty_snapshot() {
        return Ok(Manifest::default());
    }
    cas.snapshot_manifest(snapshot).map_err(|error| {
        if error.is_not_found() {
            format!(
                "the manifest of {snapshot} (data:{name}) is not in {}: name the --cas that \
                 holds the ref's snapshots",
                cas.root().display()
            )
        } else {
            error.to_string()
        }
    })
}

/// Record the snapshot `snapshot` of `args.name` after `head` (its domain's
/// head, which it differs from): its revision, kind, entry count and
/// whether the entries folded into `tree` rows.
fn record_snapshot(
    store: &PostgresStore,
    cas: &Store,
    args: &SnapshotArgs,
    head: Option<&ProvChangeRecord>,
    snapshot: &Digest,
    manifest: &Manifest,
    edges: &[ProvEdgeInput],
) -> Result<(i32, ProvChangeKind, usize, bool), String> {
    let base = match head {
        Some(head) => parse_digest(&head.result)?,
        None => empty_snapshot(),
    };
    let before = manifest_of(cas, &args.name, &base)?;
    let rows = entries(&before, manifest, None);
    let kind = if args.inputs.is_empty() {
        ProvChangeKind::Import
    } else {
        ProvChangeKind::Derive
    };
    let who = attribution(store, args.task, kind)?;
    let change = ProvChangeInput {
        change_id: change_id_of(&args.name),
        rev: head.map_or(1, |head| head.rev + 1),
        kind,
        domain: domain_of(&args.name),
        agent_id: who.agent_id,
        execution_id: None,
        task_id: who.task_id,
        preset_id: who.preset_id,
        model_label: who.model_label,
        transcript_ref: None,
        attributed: who.attributed,
        base: base.to_string(),
        result: snapshot.to_string(),
        boundary: ProvBoundary::Job,
        at_ms: now_ms(),
    };
    store
        .record_change(&change, &rows, edges)
        .map_err(|error| error.to_string())?;
    Ok((
        change.rev,
        kind,
        rows.len(),
        rows.iter().any(|row| row.class == ProvEntryClass::Tree),
    ))
}

/// `spawn prov snapshot`: the answer (module doc).
pub(crate) fn snapshot(store: &PostgresStore, args: &SnapshotArgs) -> Result<Value, String> {
    check_ref(&args.name)?;
    let cas_path = cas_dir(args.cas.as_deref())?;
    let root = check_root(Path::new(&args.dir), &cas_path)?;
    // The inputs first: a bad one writes nothing.
    let edges = used_edges(store, args)?;
    let manifest = Manifest::build(&root).map_err(|error| match error {
        CasError::SymlinkEscape { .. } => format!("refused: data root {}: {error}", root.display()),
        other => other.to_string(),
    })?;
    check_no_transcripts(&root, &manifest)?;
    let cas = Store::open(&cas_path).map_err(|error| error.to_string())?;
    let snapshot = cas
        .put_snapshot(&root, &manifest, args.digest_only)
        .map_err(|error| {
            if error.is_mismatch() {
                format!(
                    "{}: a file changed while it was read; snapshot again",
                    root.display()
                )
            } else {
                error.to_string()
            }
        })?;
    let domain = domain_of(&args.name);
    let change_id = change_id_of(&args.name);
    let mut changes = store
        .prov_domain_changes(&domain)
        .map_err(|error| error.to_string())?;
    let head = changes.first().cloned();
    let mut recorded = None;
    if head.as_ref().map(|head| head.result.as_str()) != Some(snapshot.to_string().as_str()) {
        recorded = Some(record_snapshot(
            store,
            &cas,
            args,
            head.as_ref(),
            &snapshot,
            &manifest,
            &edges,
        )?);
        changes = store
            .prov_domain_changes(&domain)
            .map_err(|error| error.to_string())?;
    }
    let kept = cas
        .keep_snapshots(&args.name, &newest_results(&changes))
        .map_err(|error| error.to_string())?;
    // A snapshot another ref's pin dropped meanwhile may have taken a
    // shared object: store what is missing again.
    if !args.digest_only
        && !cas
            .snapshot_missing(&snapshot)
            .map_err(|error| error.to_string())?
            .is_empty()
    {
        cas.put_snapshot(&root, &manifest, false)
            .map_err(|error| error.to_string())?;
    }
    let digests = |list: &[Digest]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    Ok(json!({
        "ref": args.name,
        "domain": domain,
        "change_id": change_id,
        "snapshot": snapshot.to_string(),
        "files": manifest.len(),
        "bytes": manifest.bytes(),
        "digest_only": args.digest_only,
        "recorded": recorded.is_some(),
        "rev": recorded.map(|(rev, ..)| rev).or_else(|| head.as_ref().map(|head| head.rev)),
        "kind": recorded.map(|(_, kind, ..)| kind.as_str()),
        "entries": recorded.map_or(0, |(_, _, rows, _)| rows),
        "folded": recorded.is_some_and(|(.., folded)| folded),
        "used": edges.iter().map(|edge| json!({"dst": edge.dst, "detail": edge.detail})).collect::<Vec<_>>(),
        "kept": digests(&kept.kept),
        "dropped": digests(&kept.dropped),
    }))
}

/// `spawn prov refs`: every data ref, its head and, with a store, the
/// snapshots its pin keeps.
pub(crate) fn refs(store: &PostgresStore, cas: Option<&Store>) -> Result<Value, String> {
    let mut out = Vec::new();
    for head in store
        .prov_heads(DOMAIN_PREFIX)
        .map_err(|error| error.to_string())?
    {
        let name = head
            .head
            .domain
            .strip_prefix(DOMAIN_PREFIX)
            .unwrap_or_default()
            .to_string();
        let kept = match cas {
            Some(cas) if check_ref(&name).is_ok() => Some(
                cas.kept_snapshots(&name)
                    .map_err(|error| error.to_string())?
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        out.push(json!({
            "ref": name,
            "domain": head.head.domain,
            "change_id": head.head.change_id,
            "head": head.head.result,
            "rev": head.head.rev,
            "kind": head.head.kind.as_str(),
            "at_ms": head.head.at_ms,
            "revisions": head.revisions,
            "kept": kept,
        }));
    }
    Ok(json!({ "refs": out }))
}

/// What a rollback would do, or why it may not.
struct Rollback {
    name: String,
    head: ProvChangeRecord,
    target: Digest,
    /// The newest change whose result is the target.
    origin: ProvChangeRecord,
    /// Why it is refused (`None`: it may go).
    refused: Option<String>,
    rows: Vec<ProvEntryInput>,
    changes: Vec<ProvChangeRecord>,
}

fn prepare_rollback(
    store: &PostgresStore,
    cas: &Store,
    name: &str,
    word: &str,
) -> Result<Option<Rollback>, String> {
    check_ref(name)?;
    let target = parse_digest(word)?;
    let domain = domain_of(name);
    let changes = store
        .prov_domain_changes(&domain)
        .map_err(|error| error.to_string())?;
    let head = changes
        .first()
        .cloned()
        .ok_or_else(|| format!("{domain} has no snapshot"))?;
    let origin = changes
        .iter()
        .find(|change| change.result == target.to_string())
        .cloned()
        .ok_or_else(|| format!("{target} was never a snapshot of {domain}"))?;
    if head.result == target.to_string() {
        return Ok(None);
    }
    let kept = cas
        .kept_snapshots(name)
        .map_err(|error| error.to_string())?;
    let refused = if kept.contains(&target) {
        match cas.snapshot_missing(&target) {
            Ok(missing) if missing.is_empty() => None,
            Ok(missing) => Some(format!(
                "rollback refused: {} file(s) of {target} (data:{name} rev {}) are not in the \
                 store (a --digest-only snapshot never stored them). Nothing was recorded.",
                missing.len(),
                origin.rev
            )),
            Err(error) if error.is_not_found() => Some(format!(
                "rollback refused: the manifest of {target} (data:{name}) is gone. Nothing was \
                 recorded."
            )),
            Err(error) => return Err(error.to_string()),
        }
    } else {
        Some(format!(
            "rollback refused: {target} (data:{name} rev {}) is no longer kept: only the \
             {KEEP_SNAPSHOTS} newest snapshots of a ref are pinned, and its bytes are gone. \
             Nothing was recorded.",
            origin.rev
        ))
    };
    let rows = if refused.is_none() {
        let before = manifest_of(cas, name, &parse_digest(&head.result)?)?;
        let after = manifest_of(cas, name, &target)?;
        entries(&before, &after, Some(origin.id))
    } else {
        Vec::new()
    };
    Ok(Some(Rollback {
        name: name.to_string(),
        head,
        target,
        origin,
        refused,
        rows,
        changes,
    }))
}

fn rollback_json(rollback: &Rollback) -> Value {
    json!({
        "ref": rollback.name,
        "domain": domain_of(&rollback.name),
        "change_id": rollback.head.change_id,
        "from": rollback.head.result,
        "from_rev": rollback.head.rev,
        "to": rollback.target.to_string(),
        "to_rev": rollback.origin.rev,
        "rev": rollback.head.rev + 1,
        "kind": ProvChangeKind::Revert.as_str(),
        "refused": rollback.refused,
        "entries": rollback.rows.iter().map(|row| json!({
            "path": row.path,
            "before": row.before,
            "after": row.after,
            "class": row.class.as_str(),
        })).collect::<Vec<_>>(),
    })
}

/// `spawn prov rollback [--plan] <ref> <snapshot>`: the exit code and the
/// answer. A plan writes nothing; a refused rollback writes nothing and
/// says why (exit 1).
pub(crate) fn rollback(
    store: &PostgresStore,
    cas: &Store,
    name: &str,
    word: &str,
    plan: bool,
    task: Option<i64>,
) -> Result<(i32, Value), String> {
    let Some(rollback) = prepare_rollback(store, cas, name, word)? else {
        return Ok((
            0,
            json!({
                "ref": name,
                "domain": domain_of(name),
                "plan": plan,
                "recorded": false,
                "head": parse_digest(word)?.to_string(),
                "note": "the snapshot is the ref's head already: nothing to do",
            }),
        ));
    };
    let mut answer = rollback_json(&rollback);
    if plan {
        answer["plan"] = json!("rollback");
        answer["writes"] = json!(
            "nothing: `spawn prov rollback <ref> <snapshot>` moves the head and records a revert \
             revision; the directory itself is not touched"
        );
        return Ok((i32::from(rollback.refused.is_some()), answer));
    }
    if let Some(refused) = &rollback.refused {
        return Err(refused.clone());
    }
    let who = attribution(store, task, ProvChangeKind::Revert)?;
    let change = ProvChangeInput {
        change_id: rollback.head.change_id.clone(),
        rev: rollback.head.rev + 1,
        kind: ProvChangeKind::Revert,
        domain: domain_of(name),
        agent_id: who.agent_id,
        execution_id: None,
        task_id: who.task_id,
        preset_id: who.preset_id,
        model_label: who.model_label,
        transcript_ref: None,
        attributed: who.attributed,
        base: rollback.head.result.clone(),
        result: rollback.target.to_string(),
        boundary: ProvBoundary::Job,
        at_ms: now_ms(),
    };
    store
        .record_change(&change, &rollback.rows, &[])
        .map_err(|error| error.to_string())?;
    let mut newest = vec![rollback.target.clone()];
    newest.extend(newest_results(&rollback.changes));
    let kept = cas
        .keep_snapshots(name, &newest)
        .map_err(|error| error.to_string())?;
    answer["recorded"] = json!(true);
    answer["kept"] = json!(
        kept.kept
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    Ok((0, answer))
}

/// The data snapshots among `changes` (by id), as `blast` lists them: each
/// ref, its snapshot and the revision that made it.
pub(crate) fn snapshot_rows(
    changes: &HashMap<i64, ProvChangeRecord>,
    order: &[(i64, i64, &'static str)],
) -> Vec<Value> {
    order
        .iter()
        .filter_map(|(id, depth, via)| {
            let change = changes.get(id)?;
            let name = change.domain.strip_prefix(DOMAIN_PREFIX)?;
            Some(json!({
                "ref": name,
                "snapshot": change.result,
                "change_id": change.change_id,
                "rev": change.rev,
                "kind": change.kind.as_str(),
                "depth": depth,
                "via": via,
            }))
        })
        .collect()
}
