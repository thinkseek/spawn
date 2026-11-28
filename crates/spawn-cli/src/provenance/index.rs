//! The Git walker of `spawn prov index` (PV2): it reads a source's history
//! and the event log, and records what it finds through
//! `spawn_prov_record_change` (docs/provenance.md).
//!
//! What it walks, in this order, oldest first:
//! 1. every first-parent commit of `main`. A commit with a `Spawn-Task`
//!    trailer is a landing: a `land` Change. A checkpoint on the
//!    first-parent line (a fast-forward) is an `edit` revision. Any other
//!    commit is an `import` (`attributed = 'imported'`). Before a merge's
//!    own Change, every `spawn checkpoint <execution_id>` commit its second
//!    parents bring in (`git rev-list <P2> ^<P1>`) is recorded: one `edit`
//!    revision each, under the landing's task;
//! 2. then every `spawn/*` branch, for its checkpoints `main` does not hold.
//!
//! Identity (decisions B2, binding): a `change_id` is derived from the
//! domain and a key ([`change_id`]); a revision is `max(rev) + 1` of its
//! Change for each commit not yet recorded. A commit already recorded in
//! the domain (its `result`, read through `spawn_commit_now`) is never
//! recorded again, so a second run records nothing and a run killed midway
//! is completed by the next one: every Change is written atomically, and a
//! landing's checkpoints are written before the landing.
//!
//! Attribution comes from the event log and the mission layer, never from
//! text an agent wrote: `host` when the execution named by the checkpoint's
//! subject is in the log, `inferred` when only the subject names it,
//! `imported` for a first-parent commit with no `Spawn-Task`. A landing's
//! trailers are the integrator's own (`landing_message` indents every line
//! of a report that could read as one), and only the last paragraph of a
//! message is read as trailers.
//!
//! PV4 adds two things to a landing's `land` Change, the same in the
//! indexer and the landing hook (which runs this walk):
//! - `supersedes` edges from the dispatcher's brief of the landing's task
//!   (its `task.assigned` record, never agent text): `reverts: task N`,
//!   `fixes: task N`, `supersedes: task N (<reason>)` ([`directives`],
//!   [`supersedes`]);
//! - with a workspace root, the files over the cap the task's workspace
//!   left out (`<root>/.oversized/<agent>`, decision D6), as `content`
//!   entries with their `sha256:` digest and size, for the landing that is
//!   main's newest commit ([`oversized_entries`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use serde_json::json;
use sha2::{Digest, Sha256};
use spawn_cas::{
    CAS_MARKER, CommitActor, CommitObject, CommitOrigin, CommitV2, Digest as CasDigest,
    Store as CasStore, WorkBoundary, WorkChange, empty_tree_digest,
};
use spawn_postgres::PostgresStore;
use spawn_postgres::provenance::{
    ProvAttributionSource, ProvBoundary, ProvChangeInput, ProvChangeKind, ProvChangeRecord,
    ProvEdgeInput, ProvEdgeKind, ProvEntryClass, ProvEntryInput, ProvTaskFacts,
};

/// git's empty tree: the base of a root commit.
pub(crate) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Entries one Change holds at most before they fold into one `tree` row
/// per top-level directory.
pub(crate) const FOLD_OVER: usize = 10_000;

/// The paths that are `generated` unless `spawn-prov` says otherwise: a
/// `Cargo.lock` at any depth, and the feature registry.
const GENERATED_FILES: [&str; 1] = ["docs/feature-map/registry.json"];

/// The agent of a Change no record names.
pub(crate) const UNKNOWN_AGENT: &str = "unknown";

/// The agent of an imported Change.
pub(crate) const IMPORTED_AGENT: &str = "imported";

/// The ref the walk starts from.
pub(crate) const MAIN: &str = "refs/heads/main";

/// Where the unlanded branches are.
pub(crate) const BRANCHES: &str = "refs/heads/spawn/";

/// A Change's `change_id` (B2): `c` and the first 26 characters of the
/// lowercase base32 of `sha256("spawn-change-v1" ‖ 0x00 ‖ domain ‖ 0x00 ‖
/// key)`.
pub(crate) fn change_id(domain: &str, key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"spawn-change-v1");
    hash.update([0]);
    hash.update(domain.as_bytes());
    hash.update([0]);
    hash.update(key.as_bytes());
    let mut id = String::from("c");
    id.push_str(&base32(&hash.finalize())[..26]);
    id
}

/// RFC 4648 base32 in lowercase, without padding.
pub(crate) fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

/// The domain of a Git source: `git:<source>`, the source as missions name
/// it.
pub(crate) fn domain_of(source: &str) -> String {
    format!("git:{source}")
}

/// The key of a task's `edit` Change.
pub(crate) fn edit_task_key(task: i64) -> String {
    format!("edit:task:{task}")
}

/// The key of the `edit` Change of a branch with no task.
pub(crate) fn edit_branch_key(branch: &str, merge_base: &str) -> String {
    format!("edit:branch:{branch}:{merge_base}")
}

/// The key of a landing's `land` Change.
pub(crate) fn land_key(task: i64, head: &str) -> String {
    format!("land:{task}:{head}")
}

/// The key of an imported commit's Change.
pub(crate) fn import_key(commit: &str) -> String {
    format!("import:{commit}")
}

/// A git object as a digest: `git1:<oid>`.
pub(crate) fn git1(oid: &str) -> String {
    format!("git1:{oid}")
}

/// A path as provenance stores it: as git holds it when it is UTF-8 with no
/// control character and does not begin with `"`; otherwise quoted as git
/// quotes it (`"`, `\\`, `\"`, `\t`, `\n`, and octal escapes for every
/// other control byte and every byte that is not UTF-8).
pub(crate) fn stored_path(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.starts_with('"')
        && !text.chars().any(char::is_control)
    {
        return text.to_string();
    }
    let mut out = String::from("\"");
    let mut rest = bytes;
    while !rest.is_empty() {
        let (good, bad) = match std::str::from_utf8(rest) {
            Ok(text) => (text, &[][..]),
            Err(error) => {
                let (good, bad) = rest.split_at(error.valid_up_to());
                (std::str::from_utf8(good).unwrap_or_default(), bad)
            }
        };
        for character in good.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                control if control.is_control() && u32::from(control) < 0x100 => {
                    let _ = write!(out, "\\{:03o}", u32::from(control));
                }
                other => out.push(other),
            }
        }
        match bad.split_first() {
            Some((byte, after)) => {
                let _ = write!(out, "\\{byte:03o}");
                rest = after;
            }
            None => rest = &[],
        }
    }
    out.push('"');
    out
}

/// A checkpoint commit, as its subject says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    /// The execution it names (`None`: a bare `spawn checkpoint`).
    pub(crate) execution: Option<String>,
}

/// The checkpoint `message`'s subject makes of its commit, `None` for a
/// commit that is no checkpoint.
pub(crate) fn checkpoint_execution(message: &str) -> Option<Checkpoint> {
    let subject = message.lines().next().unwrap_or_default().trim_end();
    if subject == "spawn checkpoint" {
        return Some(Checkpoint { execution: None });
    }
    let rest = subject.strip_prefix("spawn checkpoint ")?;
    let execution = rest.split_whitespace().next()?;
    Some(Checkpoint {
        execution: Some(execution.to_string()),
    })
}

/// The integrator's trailers of a message: `Spawn-Task` and `Spawn-Head`,
/// read from its last paragraph only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Trailers {
    pub(crate) task: Option<i64>,
    pub(crate) head: Option<String>,
}

/// [`Trailers`] of `message`.
pub(crate) fn trailers(message: &str) -> Trailers {
    let text = message.trim_end();
    let last = text.rsplit("\n\n").next().unwrap_or_default();
    if last.len() == text.len() {
        // A message of one paragraph is a subject, never trailers.
        return Trailers::default();
    }
    let mut found = Trailers::default();
    for line in last.lines() {
        if let Some(task) = line.strip_prefix("Spawn-Task: ") {
            found.task = task.trim().parse().ok();
        } else if let Some(head) = line.strip_prefix("Spawn-Head: ") {
            let head = head.trim();
            if head.len() == 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                found.head = Some(head.to_ascii_lowercase());
            }
        }
    }
    found
}

/// What a dispatcher's directive line says of an earlier task (PV4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DirectiveKind {
    /// `reverts: task N`: this task undoes task N.
    Reverts,
    /// `fixes: task N`: this task fixes task N (reported, never scored).
    Fixes,
    /// `supersedes: task N (<reason>)`, a reason that is no defect.
    Supersedes(SupersedeReason),
}

/// The reasons a `supersedes:` line may give: none is a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SupersedeReason {
    Refactor,
    Format,
    Planned,
    Requirement,
}

impl SupersedeReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Refactor => "refactor",
            Self::Format => "format",
            Self::Planned => "planned",
            Self::Requirement => "requirement",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        Some(match word.to_ascii_lowercase().as_str() {
            "refactor" => Self::Refactor,
            "format" => Self::Format,
            "planned" => Self::Planned,
            "requirement" => Self::Requirement,
            _ => return None,
        })
    }
}

/// One directive of a dispatcher's brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Directive {
    pub(crate) kind: DirectiveKind,
    pub(crate) task: i64,
}

/// The `defect` reason of a `supersedes` edge.
pub(crate) const REASON_DEFECT: &str = "defect";

/// Evidence of level 1 (decisions B6, item 2): the dispatcher's `reverts:`
/// landing restores the paths of the task it names to their `before`.
pub(crate) const EVIDENCE_LEVEL1: &str = "level1_revert";

/// Evidence of level 2: a regression test bisected to the landing. No
/// writer yet; `spawn prov durability` counts it when one exists.
pub(crate) const EVIDENCE_LEVEL2: &str = "level2_bisect";

/// A `reverts:` whose landing does not restore those paths.
pub(crate) const EVIDENCE_UNVERIFIED: &str = "unverified_revert";

/// A dispatcher's `fixes:`: reported, never scored.
pub(crate) const EVIDENCE_FIXES: &str = "dispatcher_fixes";

/// A dispatcher's `supersedes: task N (<reason>)`.
pub(crate) const EVIDENCE_SUPERSEDES: &str = "dispatcher_supersedes";

/// The directives of a dispatcher's brief, in order, each once: every line
/// that is, whole, `reverts: task N`, `fixes: task N` or `supersedes: task
/// N (refactor|format|planned|requirement)`, any case, with optional
/// spaces, an optional `#` before N, an optional list marker (`-`, `*`)
/// and an optional final `.`. Anything else on the line (quotes, words
/// around it) makes it no directive, so prose that mentions one is none.
pub(crate) fn directives(brief: &str) -> Vec<Directive> {
    let mut found: Vec<Directive> = Vec::new();
    for line in brief.lines() {
        let Some(directive) = directive(line) else {
            continue;
        };
        if !found.contains(&directive) {
            found.push(directive);
        }
    }
    found
}

/// One line as a [`Directive`], if it is one.
fn directive(line: &str) -> Option<Directive> {
    let mut text = line.trim();
    if let Some(rest) = text.strip_prefix(['-', '*'])
        && rest.starts_with(char::is_whitespace)
    {
        text = rest.trim_start();
    }
    let text = text.strip_suffix('.').unwrap_or(text).trim_end();
    let (word, rest) = text.split_once(':')?;
    let word = word.trim().to_ascii_lowercase();
    let rest = rest.trim_start();
    let task_word = rest.get(..4)?;
    if !task_word.eq_ignore_ascii_case("task") {
        return None;
    }
    let rest = rest[4..].trim_start();
    let rest = rest.strip_prefix('#').unwrap_or(rest);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || (digits < rest.len() && !rest[digits..].starts_with([' ', '\t', '('])) {
        return None;
    }
    let task: i64 = rest[..digits].parse().ok().filter(|task| *task > 0)?;
    let tail = rest[digits..].trim();
    let kind = match word.as_str() {
        "reverts" if tail.is_empty() => DirectiveKind::Reverts,
        "fixes" if tail.is_empty() => DirectiveKind::Fixes,
        "supersedes" => {
            let reason = tail.strip_prefix('(')?.strip_suffix(')')?.trim();
            DirectiveKind::Supersedes(SupersedeReason::parse(reason)?)
        }
        _ => return None,
    };
    Some(Directive { kind, task })
}

/// One path of `git diff-tree -r -z --no-renames`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawDiff {
    pub(crate) path: Vec<u8>,
    pub(crate) old_mode: String,
    pub(crate) new_mode: String,
    /// `None`: absent (an addition).
    pub(crate) old: Option<String>,
    /// `None`: absent (a deletion).
    pub(crate) new: Option<String>,
}

/// The paths of a `-z` raw diff (`:<old mode> <new mode> <old> <new>
/// <status>\0<path>\0`, repeated).
pub(crate) fn parse_raw(listing: &[u8]) -> Result<Vec<RawDiff>, String> {
    let mut fields = listing.split(|&byte| byte == 0);
    let mut diffs = Vec::new();
    while let Some(header) = fields.next() {
        if header.is_empty() {
            continue;
        }
        let header = std::str::from_utf8(header)
            .map_err(|_| "a diff header that is not text".to_string())?;
        let header = header
            .strip_prefix(':')
            .ok_or_else(|| format!("a diff header without its colon: {header:?}"))?;
        let parts: Vec<&str> = header.split(' ').collect();
        let [old_mode, new_mode, old, new, _status] = parts[..] else {
            return Err(format!("a diff header of {} fields", parts.len()));
        };
        let path = fields
            .next()
            .ok_or_else(|| "a diff header without its path".to_string())?;
        let absent = |oid: &str| oid.bytes().all(|byte| byte == b'0');
        diffs.push(RawDiff {
            path: path.to_vec(),
            old_mode: old_mode.to_string(),
            new_mode: new_mode.to_string(),
            old: (!absent(old)).then(|| old.to_string()),
            new: (!absent(new)).then(|| new.to_string()),
        });
    }
    Ok(diffs)
}

/// The class of one path's change: `spawn-prov`'s value when it is a class
/// (`attribute`), else `mechanical` for a change of mode alone, else
/// `generated` for the built-in list, else `content`.
pub(crate) fn classify(diff: &RawDiff, attribute: Option<&str>) -> ProvEntryClass {
    match attribute {
        Some("generated") => return ProvEntryClass::Generated,
        Some("mechanical") => return ProvEntryClass::Mechanical,
        Some("content") => return ProvEntryClass::Content,
        _ => {}
    }
    if diff.old.is_some() && diff.old == diff.new && diff.old_mode != diff.new_mode {
        return ProvEntryClass::Mechanical;
    }
    let path = diff.path.as_slice();
    let name = path.rsplit(|&byte| byte == b'/').next().unwrap_or(path);
    if name == b"Cargo.lock" || GENERATED_FILES.iter().any(|file| file.as_bytes() == path) {
        return ProvEntryClass::Generated;
    }
    ProvEntryClass::Content
}

/// One commit as the walker reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitInfo {
    pub(crate) id: String,
    pub(crate) parents: Vec<String>,
    pub(crate) tree: String,
    /// The committer's time, in ms.
    pub(crate) at_ms: i64,
    pub(crate) message: String,
}

/// A commit object's text as [`CommitInfo`].
pub(crate) fn parse_commit(id: &str, body: &[u8]) -> Result<CommitInfo, String> {
    let text = String::from_utf8_lossy(body);
    let (headers, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let mut parents = Vec::new();
    let mut tree = None;
    let mut at_ms = None;
    for line in headers.lines() {
        if let Some(oid) = line.strip_prefix("tree ") {
            tree = Some(oid.to_string());
        } else if let Some(oid) = line.strip_prefix("parent ") {
            parents.push(oid.to_string());
        } else if let Some(who) = line.strip_prefix("committer ") {
            let mut words = who.rsplit(' ');
            let _zone = words.next();
            at_ms = words
                .next()
                .and_then(|seconds| seconds.parse::<i64>().ok())
                .map(|seconds| seconds.saturating_mul(1000));
        }
    }
    Ok(CommitInfo {
        id: id.to_string(),
        parents,
        tree: tree.ok_or_else(|| format!("commit {id} has no tree"))?,
        at_ms: at_ms.ok_or_else(|| format!("commit {id} has no committer time"))?,
        message: message.to_string(),
    })
}

/// One header line of `git cat-file --batch-command`: `<oid> <type>
/// <size>`, or `<object> missing` / `<object> ambiguous` (`None`). The
/// object named may hold spaces (`<commit>:<a dir>`), so the answer is read
/// from its end: the last word says missing or ambiguous before anything
/// else.
pub(crate) fn parse_header(line: &str) -> Result<Option<(String, String, u64)>, String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.ends_with(" missing") || line.ends_with(" ambiguous") {
        return Ok(None);
    }
    let mut words = line.rsplitn(3, ' ');
    let (Some(size), Some(kind), Some(oid)) = (words.next(), words.next(), words.next()) else {
        return Err(format!("git cat-file answered {line:?}"));
    };
    if oid.is_empty() || oid.contains(' ') {
        return Err(format!("git cat-file answered {line:?}"));
    }
    let size = size
        .parse()
        .map_err(|_| format!("git cat-file answered {line:?}"))?;
    Ok(Some((oid.to_string(), kind.to_string(), size)))
}

/// A read of one object by a long-lived `git cat-file --batch-command`.
pub(crate) struct Objects {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Objects {
    fn start(git_dir: &str) -> Result<Self, String> {
        let mut child = crate::sources::controller_git()
            .args(["--git-dir", git_dir, "cat-file", "--batch-command"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("run git cat-file: {error}"))?;
        let input = child.stdin.take().ok_or("git cat-file has no stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("git cat-file has no stdout")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }

    /// The header of the answer to `command`: `(oid, type, size)`, `None`
    /// for a missing object.
    fn ask(&mut self, command: &str) -> Result<Option<(String, String, u64)>, String> {
        if command.contains('\n') {
            return Ok(None);
        }
        writeln!(self.input, "{command}")
            .and_then(|()| self.input.flush())
            .map_err(|error| format!("ask git cat-file: {error}"))?;
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .map_err(|error| format!("read git cat-file: {error}"))?;
        parse_header(&line)
    }

    /// The type and size of `object`, `None` when missing.
    pub(crate) fn info(&mut self, object: &str) -> Result<Option<(String, String, u64)>, String> {
        self.ask(&format!("info {object}"))
    }

    /// The bytes of `object`, `None` when missing.
    pub(crate) fn contents(&mut self, object: &str) -> Result<Option<Vec<u8>>, String> {
        let Some((_, _, size)) = self.ask(&format!("contents {object}"))? else {
            return Ok(None);
        };
        let size = usize::try_from(size).map_err(|_| format!("{object} is too large"))?;
        let mut body = vec![0; size + 1];
        self.output
            .read_exact(&mut body)
            .map_err(|error| format!("read git cat-file: {error}"))?;
        body.pop();
        Ok(Some(body))
    }
}

impl Drop for Objects {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A source's repository, read with git alone.
pub(crate) struct Repo {
    git_dir: String,
    objects: Objects,
}

impl Repo {
    pub(crate) fn open(git_dir: &str) -> Result<Self, String> {
        let repo = Self {
            git_dir: git_dir.to_string(),
            objects: Objects::start(git_dir)?,
        };
        repo.git(&["rev-parse", "--git-dir"])
            .map_err(|error| format!("{git_dir} is no git repository: {error}"))?;
        Ok(repo)
    }

    /// git's stdout, bytes and all, or its failure.
    pub(crate) fn git(&self, arguments: &[&str]) -> Result<Vec<u8>, String> {
        let output = crate::sources::controller_git()
            .args(["--git-dir", &self.git_dir])
            .args(arguments)
            .output()
            .map_err(|error| format!("run git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {}: {}",
                arguments.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    }

    /// git's stdout as text lines.
    fn lines(&self, arguments: &[&str]) -> Result<Vec<String>, String> {
        Ok(String::from_utf8_lossy(&self.git(arguments)?)
            .lines()
            .map(str::to_string)
            .filter(|line| !line.is_empty())
            .collect())
    }

    /// The commit `name` names, `None` when it names none.
    pub(crate) fn resolve(&self, name: &str) -> Option<String> {
        self.lines(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ])
        .ok()
        .and_then(|lines| lines.into_iter().next())
    }

    /// One commit.
    pub(crate) fn commit(&mut self, id: &str) -> Result<CommitInfo, String> {
        let body = self
            .objects
            .contents(id)?
            .ok_or_else(|| format!("commit {id} is missing"))?;
        parse_commit(id, &body)
    }

    /// `main`'s first-parent commits, oldest first.
    pub(crate) fn first_parents(&self, main: &str) -> Result<Vec<String>, String> {
        self.lines(&["rev-list", "--first-parent", "--reverse", main, "--"])
    }

    /// The commits `tip` holds and `exclude` does not, parents before
    /// children.
    pub(crate) fn only_in(&self, tip: &str, exclude: &str) -> Result<Vec<String>, String> {
        self.lines(&[
            "rev-list",
            "--reverse",
            "--topo-order",
            tip,
            &format!("^{exclude}"),
            "--",
        ])
    }

    /// The best common ancestor of `a` and `b`, if any.
    pub(crate) fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.lines(&["merge-base", a, b])
            .ok()
            .and_then(|lines| lines.into_iter().next())
    }

    /// Every branch under `prefix`: its name without `refs/heads/` and its
    /// commit, by name.
    pub(crate) fn branches(&self, prefix: &str) -> Result<Vec<(String, String)>, String> {
        let mut found: Vec<(String, String)> = self
            .lines(&["for-each-ref", "--format=%(objectname) %(refname)", prefix])?
            .into_iter()
            .filter_map(|line| {
                let (oid, name) = line.split_once(' ')?;
                Some((
                    name.strip_prefix("refs/heads/")?.to_string(),
                    oid.to_string(),
                ))
            })
            .collect();
        found.sort();
        Ok(found)
    }

    /// The paths that differ from `from` (a commit or tree) to `to`.
    pub(crate) fn diff(&self, from: &str, to: &str) -> Result<Vec<RawDiff>, String> {
        parse_raw(&self.git(&["diff-tree", "-r", "-z", "--no-renames", from, to])?)
    }

    /// `spawn-prov`'s value for each of `paths` in `commit`, read when the
    /// commit's tree has a `.gitattributes` at its top: every path whose
    /// value is set to a word.
    pub(crate) fn attributes(
        &mut self,
        commit: &str,
        paths: &[&[u8]],
    ) -> Result<HashMap<Vec<u8>, String>, String> {
        if paths.is_empty()
            || self
                .objects
                .info(&format!("{commit}:.gitattributes"))?
                .is_none()
        {
            return Ok(HashMap::new());
        }
        let mut child = crate::sources::controller_git()
            .args(["--git-dir", &self.git_dir, "check-attr", "-z", "--stdin"])
            .arg(format!("--source={commit}"))
            .arg("spawn-prov")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("run git check-attr: {error}"))?;
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path);
            input.push(0);
        }
        let mut stdin = child.stdin.take().ok_or("git check-attr has no stdin")?;
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let output = child
            .wait_with_output()
            .map_err(|error| format!("run git check-attr: {error}"))?;
        let _ = writer.join();
        if !output.status.success() {
            return Err(format!(
                "git check-attr: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let fields: Vec<&[u8]> = output.stdout.split(|&byte| byte == 0).collect();
        Ok(fields
            .chunks_exact(3)
            .filter_map(|triple| {
                let value = String::from_utf8_lossy(triple[2]).into_owned();
                (!matches!(value.as_str(), "unspecified" | "unset" | "set"))
                    .then(|| (triple[0].to_vec(), value))
            })
            .collect())
    }

    /// The size of blob `oid`, `None` for anything that is no blob.
    pub(crate) fn blob_size(&mut self, oid: &str) -> Result<Option<i64>, String> {
        Ok(match self.objects.info(oid)? {
            Some((_, kind, size)) if kind == "blob" => i64::try_from(size).ok(),
            _ => None,
        })
    }

    /// The object `commit` holds at `path` (a blob, a tree, a submodule's
    /// commit), `None` when it holds nothing there.
    pub(crate) fn object_at(&mut self, commit: &str, path: &str) -> Result<Option<String>, String> {
        if path.is_empty() || path.starts_with('"') {
            return Ok(None);
        }
        Ok(self
            .objects
            .info(&format!("{commit}:{path}"))?
            .map(|(oid, _, _)| oid))
    }

    /// The tree `commit` holds at the top-level directory `name`, if any.
    pub(crate) fn subtree(&mut self, commit: &str, name: &str) -> Result<Option<String>, String> {
        Ok(match self.objects.info(&format!("{commit}:{name}"))? {
            Some((oid, kind, _)) if kind == "tree" => Some(oid),
            _ => None,
        })
    }
}

/// A Change's entries from `base` to `result` (both commits; `base`
/// `None`: a root commit), only the paths where `result` differs from every
/// commit of `also` (the other parents of a merge: what came from them is
/// not this Change's), classed, sized, and folded past [`FOLD_OVER`].
pub(crate) fn entries(
    repo: &mut Repo,
    base: Option<&str>,
    result: &str,
    also: &[String],
) -> Result<Vec<ProvEntryInput>, String> {
    let mut diffs = repo.diff(base.unwrap_or(EMPTY_TREE), result)?;
    for other in also {
        let differs: HashSet<Vec<u8>> = repo
            .diff(other, result)?
            .into_iter()
            .map(|diff| diff.path)
            .collect();
        diffs.retain(|diff| differs.contains(&diff.path));
    }
    if diffs.len() > FOLD_OVER {
        return fold(repo, base, result, &diffs);
    }
    let paths: Vec<&[u8]> = diffs.iter().map(|diff| diff.path.as_slice()).collect();
    let attributes = repo.attributes(result, &paths)?;
    let mut out = Vec::with_capacity(diffs.len());
    for diff in &diffs {
        let size = match &diff.new {
            Some(oid) if diff.new_mode != "160000" => repo.blob_size(oid)?,
            _ => None,
        };
        out.push(ProvEntryInput {
            path: stored_path(&diff.path),
            before: diff.old.as_deref().map(git1),
            after: diff.new.as_deref().map(git1),
            size,
            class: classify(diff, attributes.get(&diff.path).map(String::as_str)),
            origin: None,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// [`entries`] folded: a path at the top stays a row of its own; every
/// other path folds into one `tree` row of its top-level directory, whose
/// digests are that directory's trees.
fn fold(
    repo: &mut Repo,
    base: Option<&str>,
    result: &str,
    diffs: &[RawDiff],
) -> Result<Vec<ProvEntryInput>, String> {
    let mut out = Vec::new();
    let mut directories: BTreeSet<Vec<u8>> = BTreeSet::new();
    for diff in diffs {
        match diff.path.iter().position(|&byte| byte == b'/') {
            Some(at) => {
                directories.insert(diff.path[..at].to_vec());
            }
            None => out.push(ProvEntryInput {
                path: stored_path(&diff.path),
                before: diff.old.as_deref().map(git1),
                after: diff.new.as_deref().map(git1),
                size: None,
                class: classify(diff, None),
                origin: None,
            }),
        }
    }
    for directory in directories {
        let name = String::from_utf8_lossy(&directory).into_owned();
        let before = match base {
            Some(base) => repo.subtree(base, &name)?,
            None => None,
        };
        let after = repo.subtree(result, &name)?;
        if before.is_some() && before == after {
            continue;
        }
        out.push(ProvEntryInput {
            path: stored_path(&directory),
            before: before.as_deref().map(git1),
            after: after.as_deref().map(git1),
            size: None,
            class: ProvEntryClass::Tree,
            origin: None,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Where a checkpoint was found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Found {
    /// Brought in by the merge `merge` on main: under its task when it is a
    /// landing; else the agent's task at the time, or the branch.
    Merge {
        task: Option<i64>,
        merge_base: String,
    },
    /// On main's first-parent line.
    FirstParent,
    /// On the unlanded branch `branch` (`spawn/<agent>`).
    Branch { branch: String, merge_base: String },
}

/// One thing to record, in the walk's order.
#[derive(Debug, Clone)]
enum Item {
    Checkpoint {
        commit: CommitInfo,
        execution: Option<String>,
        found: Found,
    },
    Main {
        commit: CommitInfo,
        trailers: Trailers,
        /// Every checkpoint its second parents bring in, recorded or not,
        /// parents first.
        checkpoints: Vec<String>,
    },
}

/// What one run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) edits: usize,
    pub(crate) lands: usize,
    pub(crate) imports: usize,
    /// First-parent commits of main already recorded.
    pub(crate) known: usize,
}

impl Summary {
    pub(crate) fn recorded(&self) -> usize {
        self.edits + self.lands + self.imports
    }
}

/// What the domain holds already, kept up to date as the run records.
struct Known {
    /// Each recorded `result` (as it is now) and its change's row id.
    results: HashMap<String, i64>,
    /// Each Change's highest revision.
    revs: HashMap<String, i32>,
    /// Each recorded row's `change_id`.
    change_ids: HashMap<i64, String>,
    /// The entries `(path, after)` of rows recorded by this run.
    entries: HashMap<i64, Vec<(String, Option<String>)>>,
}

impl Known {
    fn load(store: &PostgresStore, domain: &str) -> Result<Self, String> {
        let mut known = Self {
            results: HashMap::new(),
            revs: HashMap::new(),
            change_ids: HashMap::new(),
            entries: HashMap::new(),
        };
        for row in store
            .prov_recorded(domain)
            .map_err(|error| error.to_string())?
        {
            if let Some(result) = row.result_now {
                known.results.entry(result).or_insert(row.id);
            }
            let rev = known.revs.entry(row.change_id.clone()).or_insert(0);
            *rev = (*rev).max(row.rev);
            known.change_ids.insert(row.id, row.change_id);
        }
        Ok(known)
    }

    fn holds(&self, commit: &str) -> bool {
        self.results.contains_key(&git1(commit))
    }

    fn next_rev(&self, change_id: &str) -> i32 {
        self.revs.get(change_id).copied().unwrap_or(0) + 1
    }

    fn note(&mut self, id: i64, change: &ProvChangeInput, entries: &[ProvEntryInput]) {
        self.results.insert(change.result.clone(), id);
        self.revs.insert(change.change_id.clone(), change.rev);
        self.change_ids.insert(id, change.change_id.clone());
        self.entries.insert(
            id,
            entries
                .iter()
                .map(|entry| (entry.path.clone(), entry.after.clone()))
                .collect(),
        );
    }
}

/// The walk's plan: everything not yet recorded, in order.
fn plan(repo: &mut Repo, known: &Known, summary: &mut Summary) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    let Some(main) = repo.resolve(MAIN) else {
        return Err(format!("the repository has no {MAIN}"));
    };
    let mut planned: HashSet<String> = HashSet::new();
    for id in repo.first_parents(&main)? {
        if known.holds(&id) {
            summary.known += 1;
            continue;
        }
        let commit = repo.commit(&id)?;
        if let Some(Checkpoint { execution }) = checkpoint_execution(&commit.message) {
            planned.insert(id);
            items.push(Item::Checkpoint {
                commit,
                execution,
                found: Found::FirstParent,
            });
            continue;
        }
        let trailers = trailers(&commit.message);
        let mut checkpoints = Vec::new();
        if let [first, others @ ..] = commit.parents.as_slice() {
            for other in others {
                let merge_base = repo.merge_base(first, other).unwrap_or_default();
                for brought in repo.only_in(other, first)? {
                    let info = repo.commit(&brought)?;
                    let Some(Checkpoint { execution }) = checkpoint_execution(&info.message) else {
                        continue;
                    };
                    checkpoints.push(brought.clone());
                    if known.holds(&brought) || !planned.insert(brought) {
                        continue;
                    }
                    items.push(Item::Checkpoint {
                        commit: info,
                        execution,
                        found: Found::Merge {
                            task: trailers.task,
                            merge_base: merge_base.clone(),
                        },
                    });
                }
            }
        }
        items.push(Item::Main {
            commit,
            trailers,
            checkpoints,
        });
    }
    for (branch, tip) in repo.branches(BRANCHES)? {
        let commits = repo.only_in(&tip, &main)?;
        if commits.is_empty() {
            continue;
        }
        let merge_base = repo.merge_base(&main, &tip).unwrap_or_default();
        for id in commits {
            if known.holds(&id) || planned.contains(&id) {
                continue;
            }
            let commit = repo.commit(&id)?;
            let Some(Checkpoint { execution }) = checkpoint_execution(&commit.message) else {
                continue;
            };
            planned.insert(id);
            items.push(Item::Checkpoint {
                commit,
                execution,
                found: Found::Branch {
                    branch: branch.clone(),
                    merge_base: merge_base.clone(),
                },
            });
        }
    }
    Ok(items)
}

/// The event log's and the mission layer's facts the plan needs, read in
/// three statements.
struct Facts {
    /// Each execution's agent.
    executions: HashMap<String, String>,
    /// Each agent's tasks: `(created_at_ms, task)`, oldest first.
    agent_tasks: HashMap<String, Vec<(i64, i64)>>,
    tasks: HashMap<i64, ProvTaskFacts>,
    /// The dispatcher's brief of each landing's task (PV4).
    briefs: HashMap<i64, String>,
}

impl Facts {
    fn load(
        store: &PostgresStore,
        v2_commits: &HashMap<String, (CasDigest, CommitV2)>,
        items: &[Item],
    ) -> Result<Self, String> {
        let error = |error: spawn_core::port::StoreError| error.to_string();
        let mut executions: Vec<String> = items
            .iter()
            .filter_map(|item| match item {
                Item::Checkpoint { execution, .. } => execution.clone(),
                Item::Main { .. } => None,
            })
            .collect();
        executions.sort();
        executions.dedup();
        let executions = store.prov_execution_agents(&executions).map_err(error)?;
        let mut agents: Vec<String> = executions.values().cloned().collect();
        for item in items {
            if let Item::Checkpoint {
                found: Found::Branch { branch, .. },
                ..
            } = item
            {
                agents.push(branch_agent(branch).to_string());
            }
        }
        agents.sort();
        agents.dedup();
        let mut agent_tasks: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        for (agent, task, at) in store.prov_agent_tasks(&agents).map_err(error)? {
            agent_tasks.entry(agent).or_default().push((at, task));
        }
        let mut tasks: Vec<i64> = agent_tasks
            .values()
            .flatten()
            .map(|(_, task)| *task)
            .collect();
        let mut landed: Vec<i64> = Vec::new();
        for item in items {
            match item {
                Item::Main {
                    commit, trailers, ..
                } => {
                    if let Some((_, v2)) = v2_commits.get(&commit.id) {
                        // CommitV2 typed intent is authoritative; ignore any forged trailers.
                        if matches!(v2.intent.origin, CommitOrigin::Land)
                            && let Some(task) = v2.intent.task_id
                        {
                            tasks.push(task);
                            landed.push(task);
                        }
                    } else if let Some(task) = trailers.task {
                        tasks.push(task);
                        landed.push(task);
                    }
                }
                Item::Checkpoint {
                    commit,
                    found: Found::Merge { task, .. },
                    ..
                } => {
                    if let Some((_, v2)) = v2_commits.get(&commit.id) {
                        tasks.extend(v2.intent.task_id);
                    } else {
                        tasks.extend(*task);
                    }
                }
                Item::Checkpoint { .. } => {}
            }
        }
        tasks.sort_unstable();
        tasks.dedup();
        landed.sort_unstable();
        landed.dedup();
        let briefs = store.prov_task_briefs(&landed).map_err(error)?;
        let tasks = store.prov_task_facts(&tasks).map_err(error)?;
        Ok(Self {
            executions,
            agent_tasks,
            tasks,
            briefs,
        })
    }

    /// `agent`'s task at `at_ms`: its latest task created at or before it.
    fn task_at(&self, agent: &str, at_ms: i64) -> Option<i64> {
        self.agent_tasks
            .get(agent)?
            .iter()
            .rev()
            .find(|(created, _)| *created <= at_ms)
            .map(|(_, task)| *task)
    }
}

/// The agent a `spawn/<agent>` branch belongs to.
fn branch_agent(branch: &str) -> &str {
    branch.strip_prefix("spawn/").unwrap_or(branch)
}

/// A checkpoint's Change, as [`index`] records it.
fn checkpoint_change(
    domain: &str,
    known: &Known,
    facts: &Facts,
    commit: &CommitInfo,
    execution: Option<&String>,
    found: &Found,
) -> ProvChangeInput {
    let logged = execution.and_then(|execution| facts.executions.get(execution));
    let landing_task = match found {
        Found::Merge { task, .. } => *task,
        _ => None,
    };
    let fallback_agent = match found {
        Found::Branch { branch, .. } => Some(branch_agent(branch).to_string()),
        _ => landing_task
            .and_then(|task| facts.tasks.get(&task))
            .map(|task| task.agent_id.clone()),
    };
    let (agent, attributed) = match logged {
        Some(agent) => (agent.clone(), ProvAttributionSource::Host),
        None => (
            fallback_agent.unwrap_or_else(|| UNKNOWN_AGENT.to_string()),
            ProvAttributionSource::Inferred,
        ),
    };
    let task = landing_task.or_else(|| facts.task_at(&agent, commit.at_ms));
    let key = match (task, found) {
        (Some(task), _) => edit_task_key(task),
        (None, Found::Branch { branch, merge_base }) => edit_branch_key(branch, merge_base),
        (None, Found::Merge { merge_base, .. }) => {
            let branch = if agent == UNKNOWN_AGENT {
                UNKNOWN_AGENT.to_string()
            } else {
                format!("spawn/{agent}")
            };
            edit_branch_key(&branch, merge_base)
        }
        (None, Found::FirstParent) => edit_branch_key(
            "main",
            commit.parents.first().map_or(EMPTY_TREE, String::as_str),
        ),
    };
    let change_id = change_id(domain, &key);
    let facts_of_task = task.and_then(|task| facts.tasks.get(&task));
    ProvChangeInput {
        rev: known.next_rev(&change_id),
        change_id,
        kind: ProvChangeKind::Edit,
        domain: domain.to_string(),
        agent_id: agent,
        execution_id: execution.cloned(),
        task_id: task,
        preset_id: facts_of_task.and_then(|task| task.preset_id.clone()),
        model_label: facts_of_task.and_then(|task| task.model_label.clone()),
        transcript_ref: None,
        attributed,
        base: git1(commit.parents.first().map_or(EMPTY_TREE, String::as_str)),
        result: git1(&commit.id),
        boundary: ProvBoundary::Checkpoint,
        at_ms: commit.at_ms,
    }
}

/// A first-parent commit's Change: a landing's `land`, else an `import`.
fn main_change(
    domain: &str,
    known: &Known,
    facts: &Facts,
    commit: &CommitInfo,
    trailers: &Trailers,
) -> ProvChangeInput {
    let base = git1(commit.parents.first().map_or(EMPTY_TREE, String::as_str));
    let (kind, key, agent, attributed, boundary) = match trailers.task {
        Some(task) => {
            let head = trailers
                .head
                .clone()
                .or_else(|| commit.parents.get(1).cloned())
                .unwrap_or_else(|| commit.id.clone());
            let (agent, attributed) = match facts.tasks.get(&task) {
                Some(facts) => (facts.agent_id.clone(), ProvAttributionSource::Host),
                None => (UNKNOWN_AGENT.to_string(), ProvAttributionSource::Inferred),
            };
            (
                ProvChangeKind::Land,
                land_key(task, &head),
                agent,
                attributed,
                ProvBoundary::Land,
            )
        }
        None => (
            ProvChangeKind::Import,
            import_key(&commit.id),
            IMPORTED_AGENT.to_string(),
            ProvAttributionSource::Imported,
            ProvBoundary::Backfill,
        ),
    };
    let change_id = change_id(domain, &key);
    let task = trailers.task.and_then(|task| facts.tasks.get(&task));
    ProvChangeInput {
        rev: known.next_rev(&change_id),
        change_id,
        kind,
        domain: domain.to_string(),
        agent_id: agent,
        execution_id: None,
        task_id: trailers.task,
        preset_id: task.and_then(|task| task.preset_id.clone()),
        model_label: task.and_then(|task| task.model_label.clone()),
        transcript_ref: None,
        attributed,
        base,
        result: git1(&commit.id),
        boundary,
        at_ms: commit.at_ms,
    }
}

/// Structured provenance fields deterministically extracted from a canonical [`CommitV2`]
/// without any message or trailer text parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitV2ProvenanceFacts {
    pub(crate) kind: ProvChangeKind,
    pub(crate) boundary: ProvBoundary,
    pub(crate) agent_id: String,
    pub(crate) execution_id: Option<String>,
    pub(crate) task_id: Option<i64>,
    pub(crate) preset_id: Option<String>,
    pub(crate) model_label: Option<String>,
    pub(crate) transcript_ref: Option<String>,
    pub(crate) attributed: ProvAttributionSource,
    pub(crate) at_ms: i64,
}

fn project_commit_v2_origin(
    origin: CommitOrigin,
) -> (ProvChangeKind, ProvBoundary, ProvAttributionSource) {
    match origin {
        CommitOrigin::Work { change, boundary } => {
            let kind = match change {
                WorkChange::Edit => ProvChangeKind::Edit,
                WorkChange::Derive => ProvChangeKind::Derive,
                WorkChange::Revert => ProvChangeKind::Revert,
            };
            let prov_boundary = match boundary {
                WorkBoundary::Checkpoint => ProvBoundary::Checkpoint,
                WorkBoundary::Submit => ProvBoundary::Submit,
                WorkBoundary::Job => ProvBoundary::Job,
            };
            (kind, prov_boundary, ProvAttributionSource::Host)
        }
        CommitOrigin::Land => (
            ProvChangeKind::Land,
            ProvBoundary::Land,
            ProvAttributionSource::Host,
        ),
        CommitOrigin::Import => (
            ProvChangeKind::Import,
            ProvBoundary::Backfill,
            ProvAttributionSource::Imported,
        ),
    }
}

/// Purely project a canonical [`CommitV2`] into [`CommitV2ProvenanceFacts`], matching all
/// 11 [`CommitOrigin`] variants and all 3 [`CommitActor`] variants with zero text scraping.
pub(crate) fn project_commit_v2_facts(commit: &CommitV2) -> CommitV2ProvenanceFacts {
    let (kind, boundary, attributed) = project_commit_v2_origin(commit.intent.origin);
    let (agent_id, execution_id, model_label, preset_id) = match &commit.actor {
        CommitActor::Agent {
            id,
            execution_id,
            model_label,
            preset_id,
        } => (
            id.clone(),
            execution_id.clone(),
            model_label.clone(),
            preset_id.clone(),
        ),
        CommitActor::Human { id } | CommitActor::System { id } => (id.clone(), None, None, None),
    };
    CommitV2ProvenanceFacts {
        kind,
        boundary,
        agent_id,
        execution_id,
        task_id: commit.intent.task_id,
        preset_id,
        model_label,
        transcript_ref: commit.intent.transcript_ref.clone(),
        attributed,
        at_ms: commit.at_ms,
    }
}

/// Derive a deterministic `change_id` solely from structured [`CommitV2`] fields.
pub(crate) fn commit_v2_change_id(
    domain: &str,
    commit_digest: &CasDigest,
    commit: &CommitV2,
) -> String {
    let key = match (commit.intent.origin, commit.intent.task_id) {
        (CommitOrigin::Land, Some(task)) => {
            let head = commit.parents.get(1).unwrap_or(commit_digest).to_string();
            land_key(task, &head)
        }
        (CommitOrigin::Import, _) => import_key(&commit_digest.to_string()),
        (
            CommitOrigin::Work {
                change: WorkChange::Edit,
                ..
            },
            Some(task),
        ) => edit_task_key(task),
        (CommitOrigin::Work { change, .. }, Some(task)) => {
            format!("{}:task:{task}", change.as_str())
        }
        (origin, None) => {
            let kind_str = match origin {
                CommitOrigin::Work { change, .. } => change.as_str(),
                CommitOrigin::Land => "land",
                CommitOrigin::Import => "import",
            };
            format!("{kind_str}:actor:{}:{commit_digest}", commit.actor.id())
        }
    };
    change_id(domain, &key)
}

/// Purely construct a [`ProvChangeInput`] from `(commit_digest, commit)` with optional
/// `base_override` / `result_override` identifiers (e.g. `git1(...)` on Git-border paths).
pub(crate) fn project_commit_v2_change(
    domain: &str,
    commit_digest: &CasDigest,
    commit: &CommitV2,
    rev: i32,
    base_override: Option<&str>,
    result_override: Option<&str>,
) -> ProvChangeInput {
    let facts = project_commit_v2_facts(commit);
    let cid = commit_v2_change_id(domain, commit_digest, commit);
    let base = base_override.map_or_else(
        || {
            commit
                .parents
                .first()
                .cloned()
                .unwrap_or_else(empty_tree_digest)
                .to_string()
        },
        str::to_string,
    );
    let result = result_override.map_or_else(|| commit_digest.to_string(), str::to_string);
    ProvChangeInput {
        change_id: cid,
        rev,
        kind: facts.kind,
        domain: domain.to_string(),
        agent_id: facts.agent_id,
        execution_id: facts.execution_id,
        task_id: facts.task_id,
        preset_id: facts.preset_id,
        model_label: facts.model_label,
        transcript_ref: facts.transcript_ref,
        attributed: facts.attributed,
        base,
        result,
        boundary: facts.boundary,
        at_ms: facts.at_ms,
    }
}

fn open_existing_d6_cas(path: &Path) -> Result<Option<CasStore>, String> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!("inspect D6 CAS at {}: {err}", path.display()));
        }
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(format!("D6 CAS path {} is not a directory", path.display()));
            }
        }
    }
    let marker = path.join(CAS_MARKER);
    match std::fs::symlink_metadata(&marker) {
        Err(err) => {
            return Err(format!(
                "invalid D6 CAS store at {} (missing or unreadable {CAS_MARKER}: {err})",
                path.display()
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(format!(
                "invalid D6 CAS marker at {}: not a regular file",
                marker.display()
            ));
        }
        Ok(_) => {}
    }
    CasStore::open(path)
        .map(Some)
        .map_err(|err| format!("open D6 CAS at {}: {err}", path.display()))
}

fn resolve_d6_cas_dir_for_index(
    root: Option<&Path>,
    d6_cfg: &crate::config::D6RuntimeConfig,
    configured: crate::config::WorkspaceBackend,
) -> Result<Option<std::path::PathBuf>, String> {
    if let Some(dir) = &d6_cfg.cas_dir {
        return Ok(Some(dir.clone()));
    }
    let Some(root) = root else {
        return Ok(None);
    };
    match configured {
        crate::config::WorkspaceBackend::D6 => Ok(Some(
            d6_cfg.effective_cas_dir(root, spawn_host::WorkspaceBackend::D6),
        )),
        crate::config::WorkspaceBackend::D6Shadow => Ok(Some(
            d6_cfg.effective_cas_dir(root, spawn_host::WorkspaceBackend::D6Shadow),
        )),
        crate::config::WorkspaceBackend::Auto
        | crate::config::WorkspaceBackend::Git
        | crate::config::WorkspaceBackend::Cow
        | crate::config::WorkspaceBackend::Overlay => {
            if let Some(active) = spawn_host::root_backend(root)
                .map_err(|err| format!("read workspace backend under {}: {err}", root.display()))?
                && matches!(
                    active,
                    spawn_host::WorkspaceBackend::D6 | spawn_host::WorkspaceBackend::D6Shadow
                )
            {
                return Ok(Some(d6_cfg.effective_cas_dir(root, active)));
            }
            for backend in [
                spawn_host::WorkspaceBackend::D6,
                spawn_host::WorkspaceBackend::D6Shadow,
            ] {
                let candidate = d6_cfg.effective_cas_dir(root, backend);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(_) => return Ok(Some(candidate)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(format!("inspect D6 CAS at {}: {err}", candidate.display()));
                    }
                }
            }
            Ok(None)
        }
    }
}

fn open_d6_cas_with_config(
    root: Option<&Path>,
    d6_cfg: &crate::config::D6RuntimeConfig,
    configured: crate::config::WorkspaceBackend,
) -> Result<Option<CasStore>, String> {
    let Some(cas_dir) = resolve_d6_cas_dir_for_index(root, d6_cfg, configured)? else {
        return Ok(None);
    };
    open_existing_d6_cas(&cas_dir)
}

fn env_d6_resolution() -> Result<
    (
        crate::config::D6RuntimeConfig,
        crate::config::WorkspaceBackend,
    ),
    String,
> {
    let d6_cfg = crate::config::D6RuntimeConfig::from_env()?;
    let configured = match std::env::var("SPAWN_WORKSPACE_BACKEND") {
        Ok(raw) => crate::config::workspace_backend(Some(&raw))?,
        Err(std::env::VarError::NotPresent) => crate::config::WorkspaceBackend::Git,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("SPAWN_WORKSPACE_BACKEND is not valid UTF-8".to_string());
        }
    };
    Ok((d6_cfg, configured))
}

fn lookup_commit_v2(
    store: &PostgresStore,
    cas: Option<&CasStore>,
    git_oid: &str,
) -> Result<Option<(CasDigest, CommitV2)>, String> {
    let Some(row) = store
        .get_snapshot_by_oid(git_oid)
        .map_err(|error| format!("lookup D6 snapshot for commit {git_oid}: {error}"))?
    else {
        return Ok(None);
    };
    let Some(cas) = cas else {
        return Err(format!(
            "D6 snapshot {} exists for commit {git_oid}, but D6 CAS is unavailable (pass --root or set SPAWN_D6_CAS_DIR)",
            row.id
        ));
    };
    let digest = CasDigest::parse(&row.id).map_err(|error| {
        format!(
            "invalid D6 snapshot digest {:?} for commit {git_oid}: {error}",
            row.id
        )
    })?;
    match cas
        .get_commit_object(&digest)
        .map_err(|error| format!("read D6 commit object {digest} for commit {git_oid}: {error}"))?
    {
        CommitObject::V2(commit) => Ok(Some((digest, commit))),
        CommitObject::V1(_) => Ok(None),
    }
}

fn load_commit_v2_map(
    store: &PostgresStore,
    cas: Option<&CasStore>,
    items: &[Item],
) -> Result<HashMap<String, (CasDigest, CommitV2)>, String> {
    let mut out = HashMap::new();
    for item in items {
        let commit = match item {
            Item::Checkpoint { commit, .. } | Item::Main { commit, .. } => commit,
        };
        if !out.contains_key(&commit.id)
            && let Some(v2) = lookup_commit_v2(store, cas, &commit.id)?
        {
            out.insert(commit.id.clone(), v2);
        }
    }
    Ok(out)
}

fn commit_v2_for_item(
    v2_commits: &HashMap<String, (CasDigest, CommitV2)>,
    domain: &str,
    known: &Known,
    commit: &CommitInfo,
) -> Option<ProvChangeInput> {
    let (digest, v2) = v2_commits.get(&commit.id)?;
    let cid = commit_v2_change_id(domain, digest, v2);
    let rev = known.next_rev(&cid);
    let base = git1(commit.parents.first().map_or(EMPTY_TREE, String::as_str));
    let result = git1(&commit.id);
    Some(project_commit_v2_change(
        domain,
        digest,
        v2,
        rev,
        Some(&base),
        Some(&result),
    ))
}

/// A merge's `includes` edges and its entries' origins: one edge for each
/// Change that holds one of its `checkpoints`, and for each entry the last
/// of those checkpoints (in the walk's order) whose entry for the path has
/// the same `after`.
fn includes(
    store: &PostgresStore,
    known: &mut Known,
    checkpoints: &[String],
    entries: &mut [ProvEntryInput],
) -> Result<Vec<ProvEdgeInput>, String> {
    let rows: Vec<i64> = checkpoints
        .iter()
        .filter_map(|commit| known.results.get(&git1(commit)).copied())
        .collect();
    let missing: Vec<i64> = rows
        .iter()
        .copied()
        .filter(|row| !known.entries.contains_key(row))
        .collect();
    if !missing.is_empty() {
        for entry in store
            .prov_entries_of(&missing)
            .map_err(|error| error.to_string())?
        {
            known
                .entries
                .entry(entry.change)
                .or_default()
                .push((entry.path, entry.after));
        }
        for row in missing {
            known.entries.entry(row).or_default();
        }
    }
    let mut origin: HashMap<(&str, Option<&str>), i64> = HashMap::new();
    for row in &rows {
        for (path, after) in &known.entries[row] {
            origin.insert((path.as_str(), after.as_deref()), *row);
        }
    }
    for entry in entries.iter_mut() {
        if entry.after.is_some() && entry.class != ProvEntryClass::Tree {
            entry.origin = origin
                .get(&(entry.path.as_str(), entry.after.as_deref()))
                .copied();
        }
    }
    let change_ids: BTreeSet<&String> = rows
        .iter()
        .filter_map(|row| known.change_ids.get(row))
        .collect();
    Ok(change_ids
        .into_iter()
        .map(|change_id| ProvEdgeInput {
            kind: ProvEdgeKind::Includes,
            dst: change_id.clone(),
            detail: json!({"select": "all"}),
            at_ms: None,
        })
        .collect())
}

/// The `supersedes` edges of task `task`'s landing `commit` (PV4), from the
/// directives of the dispatcher's brief of `task` ([`directives`]), never
/// from anything an agent wrote. One edge for each `land` Change of the
/// task a directive names in `domain` (its `change_id`), with the detail
/// `{reason, evidence, target_task}`:
/// - `reverts: task N`: `reason: "defect"`, evidence [`EVIDENCE_LEVEL1`]
///   when the landing restores N's paths ([`restores`]), else
///   [`EVIDENCE_UNVERIFIED`];
/// - `fixes: task N`: `reason: "defect"`, evidence [`EVIDENCE_FIXES`];
/// - `supersedes: task N (<reason>)`: that reason, evidence
///   [`EVIDENCE_SUPERSEDES`].
///
/// A directive that names the task itself, or a task with no `land`
/// Change in the domain, writes nothing. The edges depend on the store's
/// rows of earlier landings and on the repository alone, so the hook and
/// the indexer write the same.
fn supersedes(
    store: &PostgresStore,
    repo: &mut Repo,
    domain: &str,
    facts: &Facts,
    task: i64,
    commit: &str,
    entries: &[ProvEntryInput],
) -> Result<Vec<ProvEdgeInput>, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    let Some(brief) = facts.briefs.get(&task) else {
        return Ok(Vec::new());
    };
    let mut edges = Vec::new();
    for Directive { kind, task: target } in directives(brief) {
        if target == task {
            continue;
        }
        let lands: Vec<ProvChangeRecord> = store
            .prov_changes_for_task(target)
            .map_err(error)?
            .into_iter()
            .filter(|change| change.kind == ProvChangeKind::Land && change.domain == domain)
            .collect();
        if lands.is_empty() {
            continue;
        }
        let (reason, evidence) = match kind {
            DirectiveKind::Reverts => {
                let restored = restores(store, repo, &lands, commit, entries)?;
                (
                    REASON_DEFECT,
                    if restored {
                        EVIDENCE_LEVEL1
                    } else {
                        EVIDENCE_UNVERIFIED
                    },
                )
            }
            DirectiveKind::Fixes => (REASON_DEFECT, EVIDENCE_FIXES),
            DirectiveKind::Supersedes(reason) => (reason.as_str(), EVIDENCE_SUPERSEDES),
        };
        let change_ids: BTreeSet<&str> = lands.iter().map(|land| land.change_id.as_str()).collect();
        for change_id in change_ids {
            edges.push(ProvEdgeInput {
                kind: ProvEdgeKind::Supersedes,
                dst: change_id.to_string(),
                detail: json!({"reason": reason, "evidence": evidence, "target_task": target}),
                at_ms: None,
            });
        }
    }
    Ok(edges)
}

/// Whether the landing `commit` (its `entries`) restores the paths the
/// `land` Changes `lands` of one task landed to their `before` digests
/// (decisions B6, item 2, level 1): for every `content`, `mechanical` or
/// `tree` path they touched, `commit` holds what the earliest of them found
/// there (absent where it was absent), and the landing touched at least
/// one of them. `generated` paths (`Cargo.lock`, the registry) are not
/// asked: they are made again, not restored. A path stored quoted cannot
/// be read back, and makes it unverified.
fn restores(
    store: &PostgresStore,
    repo: &mut Repo,
    lands: &[ProvChangeRecord],
    commit: &str,
    entries: &[ProvEntryInput],
) -> Result<bool, String> {
    let mut ids: Vec<i64> = lands.iter().map(|land| land.id).collect();
    ids.sort_unstable();
    let mut before: BTreeMap<String, Option<String>> = BTreeMap::new();
    for entry in store
        .prov_entries_of(&ids)
        .map_err(|error| error.to_string())?
    {
        if entry.class == ProvEntryClass::Generated {
            continue;
        }
        // `prov_entries_of` is by change, so the earliest land comes first.
        before.entry(entry.path).or_insert(entry.before);
    }
    if before.is_empty() {
        return Ok(false);
    }
    let touched: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    if !touches_any(before.keys().map(String::as_str), &touched) {
        return Ok(false);
    }
    for (path, digest) in &before {
        if path.starts_with('"') {
            return Ok(false);
        }
        let now = repo.object_at(commit, path)?.map(|oid| git1(&oid));
        if &now != digest {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether one of the paths `landed` (a task's entries) is one of the
/// paths `touched` (a landing's entries), where either side may be folded
/// (a `tree` row is a top-level directory, [`FOLD_OVER`]): the same path;
/// a landed file under a folded touched directory; or a landed folded
/// directory over a touched file. Folding is by top-level directory only,
/// so comparing each path's top-level directory is exact.
pub(crate) fn touches_any<'a>(landed: impl IntoIterator<Item = &'a str>, touched: &[&str]) -> bool {
    fn top(path: &str) -> &str {
        path.split_once('/').map_or(path, |(top, _)| top)
    }
    let paths: HashSet<&str> = touched.iter().copied().collect();
    let tops: HashSet<&str> = touched
        .iter()
        .filter(|path| path.contains('/'))
        .map(|path| top(path))
        .collect();
    landed.into_iter().any(|path| {
        paths.contains(path)
            || (path.contains('/') && paths.contains(top(path)))
            || (!path.contains('/') && tops.contains(path))
    })
}

/// The entries of the files over the cap (decision D6) that the workspace
/// of `agent` left out, as `<root>/.oversized/<agent>` records them
/// ([`spawn_host::workspace_rules::oversized_record`]): one `content` entry for each
/// file recorded with its digest, `after` its `sha256:` digest, `size` its
/// bytes, no `before` (git never held it) and no origin. A path the
/// landing's own entries hold, or one recorded without a digest (a record
/// written before PV4), is left out.
pub(crate) fn oversized_entries(
    root: &Path,
    agent: &str,
    entries: &[ProvEntryInput],
) -> Vec<ProvEntryInput> {
    let taken: HashSet<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    spawn_host::workspace_rules::oversized_record(root, agent)
        .into_iter()
        .filter_map(|file| {
            let digest = file.digest?;
            let path = stored_path(file.path.as_bytes());
            if taken.contains(path.as_str()) {
                return None;
            }
            Some(ProvEntryInput {
                path,
                before: None,
                after: Some(digest),
                size: i64::try_from(file.size).ok(),
                class: ProvEntryClass::Content,
                origin: None,
            })
        })
        .collect()
}

/// Index `repo`'s history into `domain`: everything not recorded yet, in
/// the walk's order (module doc), with no workspace root
/// ([`index_with`]). Returns what it recorded.
#[cfg(test)]
pub(crate) fn index(
    store: &PostgresStore,
    repo: &mut Repo,
    domain: &str,
) -> Result<Summary, String> {
    index_with(store, repo, domain, None)
}

/// [`index`], with the workspace root `root` whose `.oversized` records
/// add the files over the cap to the landing that is main's newest commit
/// ([`oversized_entries`]): the record is the workspace as it is now, so it
/// is true of that landing alone. `None`: no such entries.
pub(crate) fn index_with(
    store: &PostgresStore,
    repo: &mut Repo,
    domain: &str,
    root: Option<&Path>,
) -> Result<Summary, String> {
    let (d6_cfg, configured) = env_d6_resolution()?;
    index_with_d6_config(store, repo, domain, root, &d6_cfg, configured)
}

fn index_with_d6_config(
    store: &PostgresStore,
    repo: &mut Repo,
    domain: &str,
    root: Option<&Path>,
    d6_cfg: &crate::config::D6RuntimeConfig,
    configured: crate::config::WorkspaceBackend,
) -> Result<Summary, String> {
    let mut known = Known::load(store, domain)?;
    let mut summary = Summary::default();
    let items = plan(repo, &known, &mut summary)?;
    if items.is_empty() {
        return Ok(summary);
    }
    let cas = open_d6_cas_with_config(root, d6_cfg, configured)?;
    let v2_commits = load_commit_v2_map(store, cas.as_ref(), &items)?;
    let facts = Facts::load(store, &v2_commits, &items)?;
    let tip = repo.resolve(MAIN);
    for item in &items {
        let (change, entries, edges) = match item {
            Item::Checkpoint {
                commit,
                execution,
                found,
            } => {
                let change = commit_v2_for_item(&v2_commits, domain, &known, commit)
                    .unwrap_or_else(|| {
                        checkpoint_change(domain, &known, &facts, commit, execution.as_ref(), found)
                    });
                let entries = entries(
                    repo,
                    commit.parents.first().map(String::as_str),
                    &commit.id,
                    commit.parents.get(1..).unwrap_or_default(),
                )?;
                summary.edits += 1;
                (change, entries, Vec::new())
            }
            Item::Main {
                commit,
                trailers,
                checkpoints,
            } => {
                let change = commit_v2_for_item(&v2_commits, domain, &known, commit)
                    .unwrap_or_else(|| main_change(domain, &known, &facts, commit, trailers));
                let mut entries = entries(
                    repo,
                    commit.parents.first().map(String::as_str),
                    &commit.id,
                    &[],
                )?;
                let mut edges = includes(store, &mut known, checkpoints, &mut entries)?;
                if let (ProvChangeKind::Land, Some(task)) = (change.kind, change.task_id) {
                    edges.extend(supersedes(
                        store, repo, domain, &facts, task, &commit.id, &entries,
                    )?);
                    if let Some(root) = root
                        && tip.as_deref() == Some(commit.id.as_str())
                        && let Some(agent) = facts.tasks.get(&task).map(|task| &task.agent_id)
                    {
                        let more = oversized_entries(root, agent, &entries);
                        if !more.is_empty() {
                            entries.extend(more);
                            entries.sort_by(|a, b| a.path.cmp(&b.path));
                        }
                    }
                }
                match change.kind {
                    ProvChangeKind::Land => summary.lands += 1,
                    _ => summary.imports += 1,
                }
                (change, entries, edges)
            }
        };
        let id = store
            .record_change(&change, &entries, &edges)
            .map_err(|error| {
                format!(
                    "record {} rev {} ({}): {error}",
                    change.change_id, change.rev, change.result
                )
            })?;
        known.note(id, &change, &entries);
    }
    Ok(summary)
}

#[cfg(test)]
mod commit_v2_tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use spawn_cas::{CommitIntent, CommitOrigin, Store};
    use spawn_core::id::BranchName;
    use spawn_core::port::{Commit, WorkspaceStore};
    use spawn_host::{CasMerkleStore, D6Workspace, InMemoryRefLedger};

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spawn-cli-v2-prov-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn sample_v2(origin: CommitOrigin, actor: CommitActor, message: &str) -> CommitV2 {
        CommitV2 {
            tree: empty_tree_digest(),
            parents: vec![],
            at_ms: 1_700_500_000_000,
            actor,
            intent: CommitIntent {
                origin,
                task_id: Some(321),
                transcript_ref: Some("transcript://321".to_string()),
                message: message.to_string(),
            },
            git_compat_body: None,
        }
    }

    const ORIGIN_CASES: [(
        CommitOrigin,
        ProvChangeKind,
        ProvBoundary,
        ProvAttributionSource,
    ); 11] = [
        (
            CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Checkpoint,
            },
            ProvChangeKind::Edit,
            ProvBoundary::Checkpoint,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Submit,
            },
            ProvChangeKind::Edit,
            ProvBoundary::Submit,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Job,
            },
            ProvChangeKind::Edit,
            ProvBoundary::Job,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Derive,
                boundary: WorkBoundary::Checkpoint,
            },
            ProvChangeKind::Derive,
            ProvBoundary::Checkpoint,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Derive,
                boundary: WorkBoundary::Submit,
            },
            ProvChangeKind::Derive,
            ProvBoundary::Submit,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Derive,
                boundary: WorkBoundary::Job,
            },
            ProvChangeKind::Derive,
            ProvBoundary::Job,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Revert,
                boundary: WorkBoundary::Checkpoint,
            },
            ProvChangeKind::Revert,
            ProvBoundary::Checkpoint,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Revert,
                boundary: WorkBoundary::Submit,
            },
            ProvChangeKind::Revert,
            ProvBoundary::Submit,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Work {
                change: WorkChange::Revert,
                boundary: WorkBoundary::Job,
            },
            ProvChangeKind::Revert,
            ProvBoundary::Job,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Land,
            ProvChangeKind::Land,
            ProvBoundary::Land,
            ProvAttributionSource::Host,
        ),
        (
            CommitOrigin::Import,
            ProvChangeKind::Import,
            ProvBoundary::Backfill,
            ProvAttributionSource::Imported,
        ),
    ];

    #[test]
    fn projects_all_eleven_commit_origins_and_all_actor_variants() {
        for (origin, want_kind, want_boundary, want_attr) in ORIGIN_CASES {
            let agent = CommitActor::Agent {
                id: "worker-1".to_string(),
                execution_id: Some("exec-1".to_string()),
                model_label: Some("claude-sonnet".to_string()),
                preset_id: Some("default".to_string()),
            };
            let facts = project_commit_v2_facts(&sample_v2(origin, agent, "work message"));
            assert_eq!(facts.kind, want_kind);
            assert_eq!(facts.boundary, want_boundary);
            assert_eq!(facts.attributed, want_attr);
            assert_eq!(facts.agent_id, "worker-1");
            assert_eq!(facts.execution_id.as_deref(), Some("exec-1"));
            assert_eq!(facts.model_label.as_deref(), Some("claude-sonnet"));
            assert_eq!(facts.preset_id.as_deref(), Some("default"));
            assert_eq!(facts.task_id, Some(321));
            assert_eq!(facts.transcript_ref.as_deref(), Some("transcript://321"));
        }

        let human = CommitActor::Human {
            id: "alice".to_string(),
        };
        let human_facts = project_commit_v2_facts(&sample_v2(CommitOrigin::Land, human, "human"));
        assert_eq!(human_facts.agent_id, "alice");
        assert_eq!(human_facts.execution_id, None);
        assert_eq!(human_facts.model_label, None);
        assert_eq!(human_facts.preset_id, None);

        let system = CommitActor::System {
            id: "d6-integrator".to_string(),
        };
        let sys_facts = project_commit_v2_facts(&sample_v2(CommitOrigin::Import, system, "sys"));
        assert_eq!(sys_facts.agent_id, "d6-integrator");
        assert_eq!(sys_facts.attributed, ProvAttributionSource::Imported);
        assert_eq!(sys_facts.execution_id, None);
    }

    #[test]
    fn commit_v2_projection_ignores_fake_message_trailers() {
        let spoofed_msg = "spawn checkpoint fake-exec-id\n\n\
            Spawn-Task: 999999\nSpawn-Objective: 888888\nSpawn-Head: deadbeef\n";
        let commit = CommitV2 {
            tree: empty_tree_digest(),
            parents: vec![empty_tree_digest()],
            at_ms: 1_700_500_005_000,
            actor: CommitActor::Agent {
                id: "real-agent".to_string(),
                execution_id: Some("real-exec".to_string()),
                model_label: Some("real-model".to_string()),
                preset_id: Some("real-preset".to_string()),
            },
            intent: CommitIntent {
                origin: CommitOrigin::Work {
                    change: WorkChange::Edit,
                    boundary: WorkBoundary::Checkpoint,
                },
                task_id: Some(12),
                transcript_ref: Some("transcript://12".to_string()),
                message: spoofed_msg.to_string(),
            },
            git_compat_body: None,
        };
        let digest = spawn_cas::sha256_of(b"fake-digest");
        let change = project_commit_v2_change("git:/repo", &digest, &commit, 1, None, None);
        assert_eq!(change.kind, ProvChangeKind::Edit);
        assert_eq!(change.boundary, ProvBoundary::Checkpoint);
        assert_eq!(change.agent_id, "real-agent");
        assert_eq!(change.execution_id.as_deref(), Some("real-exec"));
        assert_eq!(change.task_id, Some(12));
        assert_eq!(change.attributed, ProvAttributionSource::Host);
    }

    #[test]
    fn d6_workspace_commit_v2_projects_deterministically_from_cas() {
        let dir = temp_dir("d6-e2e");
        let cas_dir = dir.join("cas");
        let ws_dir = dir.join("ws");
        let cas = Arc::new(CasMerkleStore::open(&cas_dir).expect("open cas"));
        let ledger = Arc::new(InMemoryRefLedger::new());
        let workspace = D6Workspace::new("proj-prov-v2", &ws_dir, cas.clone(), ledger.clone());
        let branch = BranchName::new("prov_worker").expect("branch");
        workspace.ensure_branch(&branch, None).expect("ensure");
        let ws = workspace.mount(&branch).expect("mount");
        std::fs::write(PathBuf::from(ws.path()).join("work.rs"), "fn main() {}\n")
            .expect("write file");

        let actor = CommitActor::Agent {
            id: "prov_worker".to_string(),
            execution_id: Some("exec-prov-7".to_string()),
            model_label: Some("claude-opus".to_string()),
            preset_id: Some("architect".to_string()),
        };
        let intent = CommitIntent {
            origin: CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Submit,
            },
            task_id: Some(505),
            transcript_ref: Some("transcript://505".to_string()),
            message: "implement v2 provenance\n\nSpawn-Task: 9999\n".to_string(),
        };
        let committed = workspace
            .commit_v2(&ws, &actor, &intent, "d6:submit")
            .expect("commit_v2");
        assert!(matches!(committed, Commit::Created(_)));

        let head = workspace
            .branch_snapshot(branch.as_str())
            .expect("snap")
            .expect("present");
        let raw_cas = Store::open(&cas_dir).expect("reopen raw cas");
        let CommitObject::V2(v2) = raw_cas.get_commit_object(&head.id).expect("read v2") else {
            panic!("expected CommitObject::V2");
        };
        let first = project_commit_v2_change("d6:proj-prov-v2", &head.id, &v2, 1, None, None);
        let second = project_commit_v2_change("d6:proj-prov-v2", &head.id, &v2, 1, None, None);
        assert_eq!(first, second);
        assert_eq!(first.kind, ProvChangeKind::Edit);
        assert_eq!(first.boundary, ProvBoundary::Submit);
        assert_eq!(first.agent_id, "prov_worker");
        assert_eq!(first.execution_id.as_deref(), Some("exec-prov-7"));
        assert_eq!(first.task_id, Some(505));
        assert_eq!(first.model_label.as_deref(), Some("claude-opus"));
        assert_eq!(first.preset_id.as_deref(), Some("architect"));
        assert_eq!(first.transcript_ref.as_deref(), Some("transcript://505"));
        assert_eq!(first.attributed, ProvAttributionSource::Host);

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct TempRootGuard(PathBuf);

    impl Drop for TempRootGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct PgCliRefLedger<'a> {
        pg: &'a PostgresStore,
    }

    fn to_pg_kind(kind: spawn_host::D6RefKind) -> spawn_postgres::RefKind {
        match kind {
            spawn_host::D6RefKind::Main => spawn_postgres::RefKind::Main,
            spawn_host::D6RefKind::Branch => spawn_postgres::RefKind::Branch,
            spawn_host::D6RefKind::Wip => spawn_postgres::RefKind::Wip,
            spawn_host::D6RefKind::Tag => spawn_postgres::RefKind::Tag,
        }
    }

    fn from_pg_kind(kind: spawn_postgres::RefKind) -> spawn_host::D6RefKind {
        match kind {
            spawn_postgres::RefKind::Main => spawn_host::D6RefKind::Main,
            spawn_postgres::RefKind::Branch => spawn_host::D6RefKind::Branch,
            spawn_postgres::RefKind::Wip => spawn_host::D6RefKind::Wip,
            spawn_postgres::RefKind::Tag => spawn_host::D6RefKind::Tag,
        }
    }

    impl spawn_host::RefLedger for PgCliRefLedger<'_> {
        fn record_snapshot(
            &self,
            id: &CasDigest,
            git_oid: &spawn_cas::GitOid,
            tree: &CasDigest,
            parents: &[CasDigest],
            project: &str,
            created_at_ms: i64,
        ) -> Result<CasDigest, spawn_host::D6StoreError> {
            let parent_strs: Vec<String> = parents.iter().map(CasDigest::to_string).collect();
            let out = self
                .pg
                .record_snapshot(
                    &id.to_string(),
                    &git_oid.to_hex(),
                    &tree.to_string(),
                    &parent_strs,
                    project,
                    created_at_ms,
                )
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            Ok(CasDigest::parse(&out)?)
        }

        fn get_snapshot(
            &self,
            id: &CasDigest,
        ) -> Result<Option<spawn_host::LedgerSnapshot>, spawn_host::D6StoreError> {
            let row = self
                .pg
                .get_snapshot(&id.to_string())
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            row.map(|r| {
                let parents = r
                    .parents
                    .iter()
                    .map(|p| CasDigest::parse(p))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(spawn_host::LedgerSnapshot {
                    id: CasDigest::parse(&r.id)?,
                    git_oid: spawn_cas::GitOid::parse_hex(&r.git_oid)?,
                    tree: CasDigest::parse(&r.tree)?,
                    parents,
                    project: r.project,
                    created_at_ms: r.created_at_ms,
                })
            })
            .transpose()
        }

        fn get_snapshot_by_oid(
            &self,
            git_oid: &spawn_cas::GitOid,
        ) -> Result<Option<spawn_host::LedgerSnapshot>, spawn_host::D6StoreError> {
            let row = self
                .pg
                .get_snapshot_by_oid(&git_oid.to_hex())
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            row.map(|r| {
                let parents = r
                    .parents
                    .iter()
                    .map(|p| CasDigest::parse(p))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(spawn_host::LedgerSnapshot {
                    id: CasDigest::parse(&r.id)?,
                    git_oid: spawn_cas::GitOid::parse_hex(&r.git_oid)?,
                    tree: CasDigest::parse(&r.tree)?,
                    parents,
                    project: r.project,
                    created_at_ms: r.created_at_ms,
                })
            })
            .transpose()
        }

        fn cas_ref(
            &self,
            project: &str,
            name: &str,
            kind: spawn_host::D6RefKind,
            expected: Option<&CasDigest>,
            new_snapshot: &CasDigest,
            reason: &str,
        ) -> Result<u64, spawn_host::D6StoreError> {
            let exp_str = expected.map(CasDigest::to_string);
            let new_str = new_snapshot.to_string();
            let seq = self
                .pg
                .cas_ref(
                    project,
                    name,
                    to_pg_kind(kind),
                    exp_str.as_deref(),
                    &new_str,
                    reason,
                )
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            u64::try_from(seq)
                .map_err(|_| spawn_host::D6StoreError::Ledger("negative seq".to_string()))
        }

        fn land_main_ref(
            &self,
            project: &str,
            name: &str,
            expected_base: Option<&CasDigest>,
            new_snapshot: &CasDigest,
            task_id: Option<i64>,
            reason: &str,
        ) -> Result<u64, spawn_host::D6StoreError> {
            let exp_str = expected_base.map(CasDigest::to_string);
            let new_str = new_snapshot.to_string();
            let seq = self
                .pg
                .land_main_ref(project, name, exp_str.as_deref(), &new_str, task_id, reason)
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            u64::try_from(seq)
                .map_err(|_| spawn_host::D6StoreError::Ledger("negative seq".to_string()))
        }

        fn list_refs(
            &self,
            project: Option<&str>,
        ) -> Result<Vec<spawn_host::LedgerRef>, spawn_host::D6StoreError> {
            let rows = self
                .pg
                .list_refs(project)
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            rows.into_iter()
                .map(|r| {
                    Ok(spawn_host::LedgerRef {
                        project: r.project,
                        name: r.name,
                        kind: from_pg_kind(r.kind),
                        snapshot: CasDigest::parse(&r.snapshot)?,
                        updated_at_ms: r.updated_at_ms,
                    })
                })
                .collect()
        }

        fn retire_project_refs(
            &self,
            project: &str,
            reason: &str,
        ) -> Result<usize, spawn_host::D6StoreError> {
            let count = self
                .pg
                .retire_project_refs(project, reason)
                .map_err(|e| spawn_host::D6StoreError::Ledger(e.to_string()))?;
            usize::try_from(count)
                .map_err(|_| spawn_host::D6StoreError::Ledger(format!("negative count: {count}")))
        }
    }

    struct V2ProvFixture {
        _guard: TempRootGuard,
        _schema: spawn_postgres::testing::TestSchema,
        store: PostgresStore,
        schema_name: String,
        db_url: String,
        root: PathBuf,
        cas_dir: PathBuf,
        mirror_dir: PathBuf,
        domain: String,
        task_11: i64,
        task_12: i64,
        land_12_snap: CasDigest,
    }

    fn owner_client(url: &str, schema: &str) -> postgres::Client {
        let mut client = postgres::Client::connect(url, postgres::NoTls).expect("pg owner");
        client
            .batch_execute(&format!("set search_path to {schema}"))
            .expect("search_path");
        client
    }

    fn write_v2_oversized_records(root: &Path, agent_b: &str) {
        let cas_bytes = b"oversized payload\n";
        let oversized_digest = format!(
            "sha256:{}",
            spawn_cas::sha256_reader(&mut &cas_bytes[..])
                .expect("hash")
                .hex()
        );
        std::fs::create_dir_all(root.join(".oversized")).expect("mkdir oversized");
        std::fs::write(
            root.join(".oversized").join(agent_b),
            format!(
                "untold\0{} {oversized_digest}\0model.bin\0",
                cas_bytes.len()
            ),
        )
        .expect("write real oversized");
        std::fs::write(
            root.join(".oversized/agent_decoy"),
            format!(
                "untold\0{} {oversized_digest}\0decoy.bin\0",
                cas_bytes.len()
            ),
        )
        .expect("write decoy oversized");
    }

    fn seed_v2_prov_fixture(tag: &str, cas_rel: &[&str]) -> V2ProvFixture {
        let root = temp_dir(tag);
        let guard = TempRootGuard(root.clone());
        let cas_dir = cas_rel.iter().fold(root.clone(), |acc, p| acc.join(p));
        let cas = CasMerkleStore::open(&cas_dir).expect("open d6 cas");
        let mirror_dir = root.join(".sources").join("proj-v2.git");
        let source = "/srv/proj-v2.git";
        let domain = domain_of(source);
        let schema = spawn_postgres::testing::TestSchema::new("prov_v2_r2");
        let store = schema.store();
        let ledger = PgCliRefLedger { pg: &store };

        let mission = store
            .mission_add("D6 V2 index_with E2E", &json!([source]), "human")
            .expect("mission_add");
        store
            .mission_configure(mission, Some("/scenarios"), "gate", None, None)
            .expect("mission_configure");

        // Publish base CommitV2 on main.
        let (base_blob, base_sz) = cas.put_bytes_blob(b"one\n").expect("put base blob");
        let base_tree = cas
            .apply_mutations(
                &empty_tree_digest(),
                &[spawn_cas::TreeMutation::Upsert {
                    path: vec![b"a.rs".to_vec()],
                    kind: spawn_cas::TreeEntryKind::File,
                    id: base_blob.as_bytes(),
                    size: base_sz,
                }],
            )
            .expect("base tree");
        let base_actor = CommitActor::System {
            id: "d6-bootstrap".to_string(),
        };
        let base_intent = CommitIntent {
            origin: CommitOrigin::Import,
            task_id: None,
            transcript_ref: None,
            message: "Initial import".to_string(),
        };
        let base_pub = cas
            .publish_commit_v2(
                &ledger,
                &spawn_host::cas_merkle::PublishCommitV2Request {
                    project: "proj-v2",
                    ref_name: "main",
                    kind: spawn_host::D6RefKind::Main,
                    expected_ref: None,
                    tree: &base_tree,
                    parents: &[],
                    created_at_ms: 1_700_000_000_000,
                    actor: &base_actor,
                    intent: &base_intent,
                    git_compat_body: None,
                    reason: "bootstrap",
                    landing_task_id: None,
                },
            )
            .expect("publish base");

        let (t1_pub, task_11) =
            publish_v2_land_one(&cas, &store, &ledger, mission, &base_pub, &base_tree);
        let (t2_pub, task_12) =
            publish_v2_land_two(&cas, &store, &ledger, mission, &t1_pub, &base_tree, task_11);

        let agent_b = store
            .prov_task_facts(&[task_12])
            .expect("task_12 facts")
            .remove(&task_12)
            .expect("task_12 present")
            .agent_id;
        write_v2_oversized_records(&root, &agent_b);

        let mut client = owner_client(schema.url(), schema.name());
        client
            .execute(
                "insert into mission_tasks (id, mission_id, kind, agent_id, session_id, brief,
                                            created_by, created_at_ms, updated_at_ms)
                 overriding system value
                 values (99999, $1, 'ephemeral', 'agent_decoy', 'session_decoy', 'decoy', 'human', 1000, 1000)",
                &[&mission],
            )
            .expect("insert decoy task 99999");

        spawn_host::GitBorderAdapter::new()
            .export_project(&cas, &ledger, "proj-v2", &mirror_dir)
            .expect("export mirror");

        V2ProvFixture {
            _guard: guard,
            schema_name: schema.name().to_string(),
            db_url: schema.url().to_string(),
            _schema: schema,
            store,
            root,
            cas_dir,
            mirror_dir,
            domain,
            task_11,
            task_12,
            land_12_snap: t2_pub.snapshot,
        }
    }

    fn accept_task_for_commit(
        store: &PostgresStore,
        task: i64,
        base_oid: spawn_cas::GitOid,
        cand_oid: spawn_cas::GitOid,
    ) {
        store.task_delivered(task, 1).expect("task_delivered");
        assert!(store.task_reported(task, "done").expect("task_reported"));
        store
            .integrator_record(&json!({
                "type": "task.gated",
                "id": task,
                "revision": 0,
                "verdict": "accepted",
                "base": base_oid.to_hex(),
                "candidate": cand_oid.to_hex(),
            }))
            .expect("integrator_record accepted");
    }

    fn publish_v2_land_one(
        cas: &CasMerkleStore,
        store: &PostgresStore,
        ledger: &PgCliRefLedger<'_>,
        mission: i64,
        base_pub: &spawn_host::PublishedSnapshot,
        base_tree: &CasDigest,
    ) -> (spawn_host::PublishedSnapshot, i64) {
        let task_11 = store
            .task_assign(mission, "ephemeral", "worker_a", "first task", "default")
            .expect("assign task 11");
        let (t1_blob, t1_sz) = cas.put_bytes_blob(b"one\nbad\n").expect("put t1 blob");
        let t1_tree = cas
            .apply_mutations(
                base_tree,
                &[spawn_cas::TreeMutation::Upsert {
                    path: vec![b"a.rs".to_vec()],
                    kind: spawn_cas::TreeEntryKind::File,
                    id: t1_blob.as_bytes(),
                    size: t1_sz,
                }],
            )
            .expect("t1 tree");
        let t1_actor = CommitActor::Agent {
            id: "agent_a".to_string(),
            execution_id: Some("exec-11".to_string()),
            model_label: Some("claude-sonnet".to_string()),
            preset_id: Some("worker".to_string()),
        };
        let t1_intent = CommitIntent {
            origin: CommitOrigin::Land,
            task_id: Some(task_11),
            transcript_ref: Some("transcript://11".to_string()),
            message: format!("land {task_11}\n"),
        };
        let t1_v2 = CommitV2 {
            tree: t1_tree.clone(),
            parents: vec![base_pub.snapshot.clone()],
            at_ms: 1_700_000_001_000,
            actor: t1_actor.clone(),
            intent: t1_intent.clone(),
            git_compat_body: None,
        };
        let t1_oid = cas
            .project_commit_object_oid(&CommitObject::V2(t1_v2), &[base_pub.git_oid])
            .expect("project t1 oid");
        accept_task_for_commit(store, task_11, base_pub.git_oid, t1_oid);
        let pub1 = cas
            .publish_commit_v2(
                ledger,
                &spawn_host::cas_merkle::PublishCommitV2Request {
                    project: "proj-v2",
                    ref_name: "main",
                    kind: spawn_host::D6RefKind::Main,
                    expected_ref: Some(&base_pub.snapshot),
                    tree: &t1_tree,
                    parents: std::slice::from_ref(&base_pub.snapshot),
                    created_at_ms: 1_700_000_001_000,
                    actor: &t1_actor,
                    intent: &t1_intent,
                    git_compat_body: None,
                    reason: "land-11",
                    landing_task_id: Some(task_11),
                },
            )
            .expect("publish t1");
        (pub1, task_11)
    }

    fn publish_v2_land_two(
        cas: &CasMerkleStore,
        store: &PostgresStore,
        ledger: &PgCliRefLedger<'_>,
        mission: i64,
        t1_pub: &spawn_host::PublishedSnapshot,
        base_tree: &CasDigest,
        task_11: i64,
    ) -> (spawn_host::PublishedSnapshot, i64) {
        let task_12_brief = format!("Undo {task_11}\n\nfixes: task {task_11}");
        let task_12 = store
            .task_assign(mission, "ephemeral", "worker_b", &task_12_brief, "default")
            .expect("assign task 12");
        let t2_actor = CommitActor::Agent {
            id: "agent_b".to_string(),
            execution_id: Some("exec-12".to_string()),
            model_label: Some("claude-opus".to_string()),
            preset_id: Some("architect".to_string()),
        };
        let forged_msg =
            format!("land real task\n\nreverts: task {task_11}\n\nSpawn-Task: 99999\n");
        let t2_intent = CommitIntent {
            origin: CommitOrigin::Land,
            task_id: Some(task_12),
            transcript_ref: Some("transcript://12".to_string()),
            message: forged_msg,
        };
        let t2_v2 = CommitV2 {
            tree: base_tree.clone(),
            parents: vec![t1_pub.snapshot.clone()],
            at_ms: 1_700_000_002_000,
            actor: t2_actor.clone(),
            intent: t2_intent.clone(),
            git_compat_body: None,
        };
        let t2_oid = cas
            .project_commit_object_oid(&CommitObject::V2(t2_v2), &[t1_pub.git_oid])
            .expect("project t2 oid");
        accept_task_for_commit(store, task_12, t1_pub.git_oid, t2_oid);
        let pub2 = cas
            .publish_commit_v2(
                ledger,
                &spawn_host::cas_merkle::PublishCommitV2Request {
                    project: "proj-v2",
                    ref_name: "main",
                    kind: spawn_host::D6RefKind::Main,
                    expected_ref: Some(&t1_pub.snapshot),
                    tree: base_tree,
                    parents: std::slice::from_ref(&t1_pub.snapshot),
                    created_at_ms: 1_700_000_002_000,
                    actor: &t2_actor,
                    intent: &t2_intent,
                    git_compat_body: None,
                    reason: "land-12",
                    landing_task_id: Some(task_12),
                },
            )
            .expect("publish t2");
        (pub2, task_12)
    }

    #[test]
    fn index_with_v2_land_ignores_forged_trailer_for_facts_supersedes_and_oversized() {
        let fx = seed_v2_prov_fixture("v2-real-e2e", &[".d6", "cas"]);
        let mut repo = Repo::open(&fx.mirror_dir.to_string_lossy()).expect("open mirror");
        let summary = index_with(&fx.store, &mut repo, &fx.domain, Some(&fx.root))
            .expect("index_with must load V2 from .d6/cas");
        assert_eq!(summary.imports, 1);
        assert_eq!(summary.lands, 2);

        let mut client = owner_client(&fx.db_url, &fx.schema_name);
        let land_rows = client
            .query(
                "select id, task_id, agent_id, model_label, preset_id, transcript_ref
                   from prov_changes
                  where domain = $1 and kind = 'land'
                  order by id",
                &[&fx.domain],
            )
            .expect("query prov_changes");
        assert_eq!(land_rows.len(), 2);
        let land_11_id: i64 = land_rows[0].get(0);
        let land_12_id: i64 = land_rows[1].get(0);
        assert_eq!(land_rows[1].get::<_, Option<i64>>(1), Some(fx.task_12));
        assert_eq!(land_rows[1].get::<_, String>(2), "agent_b");
        assert_eq!(
            land_rows[1].get::<_, Option<String>>(3).as_deref(),
            Some("claude-opus")
        );
        assert_eq!(
            land_rows[1].get::<_, Option<String>>(4).as_deref(),
            Some("architect")
        );
        assert_eq!(
            land_rows[1].get::<_, Option<String>>(5).as_deref(),
            Some("transcript://12")
        );

        let supersedes_edges = client
            .query(
                "select dst, detail from prov_edges where src = $1 and kind = 'supersedes'",
                &[&land_12_id],
            )
            .expect("query supersedes");
        assert_eq!(supersedes_edges.len(), 1);
        let dst_change: String = supersedes_edges[0].get(0);
        let first_change_id: String = client
            .query_one(
                "select change_id from prov_changes where id = $1",
                &[&land_11_id],
            )
            .expect("land_11 cid")
            .get(0);
        assert_eq!(dst_change, first_change_id);
        let detail: serde_json::Value = supersedes_edges[0].get(1);
        assert_eq!(detail["target_task"], json!(fx.task_11));

        let entries = fx
            .store
            .prov_entries_of(&[land_12_id])
            .expect("query entries");
        assert!(
            entries
                .iter()
                .any(|e| e.path == "model.bin" && e.after.is_some())
        );
        assert!(!entries.iter().any(|e| e.path == "decoy.bin"));
    }

    #[test]
    fn index_with_d6_cas_dir_override_and_shadow_layout_resolve_v2() {
        // Explicit `SPAWN_D6_CAS_DIR` / D6RuntimeConfig cas_dir override outside root.
        let fx_custom = seed_v2_prov_fixture("v2-override", &["external-cas"]);
        let cfg = crate::config::D6RuntimeConfig::parse(
            Some(&fx_custom.cas_dir.to_string_lossy()),
            None,
            None,
            None,
        )
        .expect("parse custom cas dir");
        let mut repo = Repo::open(&fx_custom.mirror_dir.to_string_lossy()).expect("open mirror");
        let summary = index_with_d6_config(
            &fx_custom.store,
            &mut repo,
            &fx_custom.domain,
            Some(&fx_custom.root),
            &cfg,
            crate::config::WorkspaceBackend::D6,
        )
        .expect("index with explicit cas_dir override");
        assert_eq!(summary.lands, 2);

        let changes = fx_custom
            .store
            .prov_changes_for_task(fx_custom.task_12)
            .expect("changes for task 12");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].agent_id, "agent_b");

        // `.d6-shadow/cas` layout under `WorkspaceBackend::D6Shadow` and auto discovery.
        let fx_shadow = seed_v2_prov_fixture("v2-shadow", &[".d6-shadow", "cas"]);
        let mut shadow_repo =
            Repo::open(&fx_shadow.mirror_dir.to_string_lossy()).expect("open shadow mirror");
        let shadow_sum = index_with(
            &fx_shadow.store,
            &mut shadow_repo,
            &fx_shadow.domain,
            Some(&fx_shadow.root),
        )
        .expect("index discovers .d6-shadow/cas");
        assert_eq!(shadow_sum.lands, 2);
        let shadow_changes = fx_shadow
            .store
            .prov_changes_for_task(fx_shadow.task_12)
            .expect("shadow changes for task 12");
        assert_eq!(shadow_changes.len(), 1);
    }

    #[test]
    fn index_with_fails_closed_on_corrupt_d6_cas_malformed_v2_or_missing_cas() {
        use std::os::unix::fs::PermissionsExt;

        // Sub-case A: snapshot exists in PG, `root = None` -> missing CAS must error.
        let fx_missing = seed_v2_prov_fixture("v2-fail-missing", &[".d6", "cas"]);
        let mut repo_missing =
            Repo::open(&fx_missing.mirror_dir.to_string_lossy()).expect("open mirror");
        let err_missing = index_with(
            &fx_missing.store,
            &mut repo_missing,
            &fx_missing.domain,
            None,
        )
        .expect_err("missing CAS with PG snapshots must fail closed");
        assert!(
            err_missing.contains("D6 CAS is unavailable"),
            "{err_missing}"
        );

        // Sub-case B: corrupt CAS dir (missing `.spawn-cas` marker) -> must fail closed.
        let fx_marker = seed_v2_prov_fixture("v2-fail-marker", &[".d6", "cas"]);
        std::fs::remove_file(fx_marker.cas_dir.join(CAS_MARKER)).expect("rm .spawn-cas");
        let mut repo_marker =
            Repo::open(&fx_marker.mirror_dir.to_string_lossy()).expect("open mirror");
        let err_marker = index_with(
            &fx_marker.store,
            &mut repo_marker,
            &fx_marker.domain,
            Some(&fx_marker.root),
        )
        .expect_err("corrupt CAS dir must fail closed");
        assert!(err_marker.contains(CAS_MARKER), "{err_marker}");

        // Sub-case C: corrupt stored `CommitV2` object bytes on disk -> must fail closed, never fall back to forged trailer.
        let fx_corrupt = seed_v2_prov_fixture("v2-fail-obj", &[".d6", "cas"]);
        let raw_cas = Store::open(&fx_corrupt.cas_dir).expect("reopen cas");
        let obj_path = raw_cas.object_path(&fx_corrupt.land_12_snap);
        std::fs::set_permissions(&obj_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod rw");
        std::fs::write(&obj_path, b"corrupt-v2-bytes").expect("overwrite corrupt bytes");
        std::fs::set_permissions(&obj_path, std::fs::Permissions::from_mode(0o444))
            .expect("chmod ro");
        let mut repo_corrupt =
            Repo::open(&fx_corrupt.mirror_dir.to_string_lossy()).expect("open mirror");
        let err_corrupt = index_with(
            &fx_corrupt.store,
            &mut repo_corrupt,
            &fx_corrupt.domain,
            Some(&fx_corrupt.root),
        )
        .expect_err("corrupt V2 object must fail closed");
        assert!(
            err_corrupt.contains("read D6 commit object"),
            "{err_corrupt}"
        );

        // Zero partial writes into `prov_changes`.
        let mut client = owner_client(&fx_corrupt.db_url, &fx_corrupt.schema_name);
        let count: i64 = client
            .query_one(
                "select count(*) from prov_changes where domain = $1",
                &[&fx_corrupt.domain],
            )
            .expect("count")
            .get(0);
        assert_eq!(count, 0);
    }

    #[test]
    fn index_with_legacy_non_d6_repo_without_d6_cas_uses_trailers() {
        let root = temp_dir("legacy-non-d6");
        let _guard = TempRootGuard(root.clone());
        let mirror = root.join(".sources").join("legacy.git");
        std::fs::create_dir_all(mirror.parent().expect("parent")).expect("mkdir");
        let work = root.join("work");
        std::fs::create_dir_all(&work).expect("work");

        let run_git = |dir: &Path, args: &[&str]| {
            let out = spawn_host::test_git_command()
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run_git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("f.rs"), "base\n").expect("write");
        run_git(&work, &["add", "-A"]);
        run_git(&work, &["commit", "-qm", "Initial import"]);
        run_git(&work, &["checkout", "-q", "-b", "agent_legacy"]);
        std::fs::write(work.join("f.rs"), "updated\n").expect("write");
        run_git(&work, &["commit", "-qam", "spawn checkpoint exec_legacy"]);
        let head = run_git(&work, &["rev-parse", "HEAD"]);
        run_git(&work, &["checkout", "-q", "main"]);

        let schema = spawn_postgres::testing::TestSchema::new("prov_legacy_r2");
        let store = schema.store();
        let mission = store
            .mission_add("Legacy trailer test", &json!(["/srv/legacy.git"]), "human")
            .expect("mission");
        let task = store
            .task_assign(
                mission,
                "ephemeral",
                "agent_legacy",
                "legacy brief",
                "default",
            )
            .expect("task");

        run_git(
            &work,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("Task {task}\n\nSpawn-Task: {task}\nSpawn-Head: {head}\n"),
                "agent_legacy",
            ],
        );
        run_git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                &work.to_string_lossy(),
                &mirror.to_string_lossy(),
            ],
        );

        let domain = domain_of("/srv/legacy.git");
        let mut repo = Repo::open(&mirror.to_string_lossy()).expect("open mirror");
        let summary =
            index_with(&store, &mut repo, &domain, Some(&root)).expect("legacy trailer indexing");
        assert_eq!(summary.imports, 1);
        assert_eq!(summary.lands, 1);
        assert!(!root.join(".d6").exists());

        let changes = store.prov_changes_for_task(task).expect("changes");
        assert!(changes.iter().any(|c| c.kind == ProvChangeKind::Land));
    }
}
