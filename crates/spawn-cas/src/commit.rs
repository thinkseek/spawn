//! Canonical `spawn-commit-v2` object codec, AI provenance projection, and dual
//! `V1`/`V2` [`CommitObject`] decoding (`Wave-1 S1`).
//!
//! `spawn-commit-v2` unifies code state (`tree`, `parents`, `at_ms`) and AI
//! execution provenance (`actor`, `intent`) under a single SHA-256 content
//! address, while isolating any legacy raw Git commit metadata behind an
//! optional `git_compat` trailer that native Spawn commits omit completely.

use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::chunker::CHUNK_MAX;
use crate::merkle::SNAPSHOT_MAGIC;
use crate::{CasError, Digest, NS_TREE, Snapshot, Store, sha256_of};

/// Magic header for canonical `spawn-commit-v2` commit objects.
pub const COMMIT_V2_MAGIC: &[u8] = b"spawn-commit-v2\n";

/// Maximum parent commits permitted in a single `CommitV2` object.
const MAX_PARENTS: usize = 1_024;

/// Maximum UTF-8 byte length for a single-line header field value.
const MAX_HEADER_VALUE_LEN: usize = 1_024;

/// Closed sum type over historical `spawn-snapshot-v1` and unified
/// `spawn-commit-v2` commit objects stored in `spawn-cas`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitObject {
    /// Legacy `spawn-snapshot-v1` Git-envelope commit object.
    V1(Snapshot),
    /// Self-proving `spawn-commit-v2` code-state + AI-provenance commit object.
    V2(CommitV2),
}

impl CommitObject {
    /// Decode either a `spawn-snapshot-v1` or `spawn-commit-v2` object strictly
    /// by its canonical magic header.
    pub fn decode(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.starts_with(SNAPSHOT_MAGIC) {
            Snapshot::decode(bytes).map(Self::V1)
        } else if bytes.starts_with(COMMIT_V2_MAGIC) {
            CommitV2::decode(bytes).map(Self::V2)
        } else {
            Err(CasError::BadFormat("unknown commit object header"))
        }
    }

    /// Encode this commit object into its canonical wire representation.
    pub fn encode(&self) -> Result<Vec<u8>, CasError> {
        match self {
            Self::V1(snapshot) => snapshot.encode(),
            Self::V2(commit) => commit.encode(),
        }
    }

    /// Root `spawn-tree-v1` SHA-256 digest of this commit object.
    pub const fn tree(&self) -> &Digest {
        match self {
            Self::V1(snapshot) => &snapshot.tree,
            Self::V2(commit) => &commit.tree,
        }
    }

    /// Parent commit digests in first-parent order.
    pub fn parents(&self) -> &[Digest] {
        match self {
            Self::V1(snapshot) => &snapshot.parents,
            Self::V2(commit) => &commit.parents,
        }
    }

    /// Borrow the underlying `V1` [`Snapshot`] if this object is `V1`.
    pub const fn as_v1(&self) -> Option<&Snapshot> {
        match self {
            Self::V1(snapshot) => Some(snapshot),
            Self::V2(_) => None,
        }
    }

    /// Borrow the underlying [`CommitV2`] if this object is `V2`.
    pub const fn as_v2(&self) -> Option<&CommitV2> {
        match self {
            Self::V1(_) => None,
            Self::V2(commit) => Some(commit),
        }
    }
}

/// Closed semantic classification of how a `CommitV2` state was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    /// Direct workspace edit or tool mutation commit.
    Edit,
    /// Derived tree transformation (merge resolution, rebase, patch apply).
    Derive,
    /// Landing / integration commit advancing a canonical target branch/ref.
    Land,
    /// Compensating revert commit restoring an earlier code state.
    Revert,
    /// Initial or external history import into CAS.
    Import,
}

impl CommitKind {
    /// Canonical lowercase wire string for `kind`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Derive => "derive",
            Self::Land => "land",
            Self::Revert => "revert",
            Self::Import => "import",
        }
    }

    /// Parse strictly from the canonical lowercase wire representation.
    pub fn parse(text: &str) -> Result<Self, CasError> {
        match text {
            "edit" => Ok(Self::Edit),
            "derive" => Ok(Self::Derive),
            "land" => Ok(Self::Land),
            "revert" => Ok(Self::Revert),
            "import" => Ok(Self::Import),
            _ => Err(CasError::BadFormat("unknown commit kind")),
        }
    }
}

impl fmt::Display for CommitKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CommitKind {
    type Err = CasError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Closed execution lifecycle boundary at which a `CommitV2` was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitBoundary {
    /// Periodic or pre-handoff workspace checkpoint.
    Checkpoint,
    /// Worker completion / submission snapshot handed back to control plane.
    Submit,
    /// Control-plane integration landing onto target ref.
    Land,
    /// Automated background/scheduled job boundary.
    Job,
    /// Historical commit backfill or migration.
    Backfill,
}

impl CommitBoundary {
    /// Canonical lowercase wire string for `boundary`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::Submit => "submit",
            Self::Land => "land",
            Self::Job => "job",
            Self::Backfill => "backfill",
        }
    }

    /// Parse strictly from the canonical lowercase wire representation.
    pub fn parse(text: &str) -> Result<Self, CasError> {
        match text {
            "checkpoint" => Ok(Self::Checkpoint),
            "submit" => Ok(Self::Submit),
            "land" => Ok(Self::Land),
            "job" => Ok(Self::Job),
            "backfill" => Ok(Self::Backfill),
            _ => Err(CasError::BadFormat("unknown commit boundary")),
        }
    }
}

impl fmt::Display for CommitBoundary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CommitBoundary {
    type Err = CasError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Actor provenance recorded inside a canonical `spawn-commit-v2` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitActor {
    /// Stable identifier of the agent or human actor that produced the commit.
    pub agent_id: String,
    /// Optional execution attempt identifier when produced by a managed run.
    pub execution_id: Option<String>,
    /// Optional canonical model label active for the producing step.
    pub model_label: Option<String>,
    /// Optional preset/profile identifier active for the producing step.
    pub preset_id: Option<String>,
}

/// Intent and ledger linkage recorded inside a canonical `spawn-commit-v2` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitIntent {
    /// Closed semantic kind of commit.
    pub kind: CommitKind,
    /// Closed workflow lifecycle boundary at which the commit was created.
    pub boundary: CommitBoundary,
    /// Optional mission/task ID linked to this commit.
    pub task_id: Option<i64>,
    /// Optional transcript digest/URI reference linking back to reasoning logs.
    pub transcript_ref: Option<String>,
    /// Required non-empty human/agent commit message (UTF-8, `\n` and `\t` allowed).
    pub message: String,
}

/// Unified content-addressed commit object carrying tree state and AI lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitV2 {
    /// Root `spawn-tree-v1` SHA-256 digest.
    pub tree: Digest,
    /// Parent commit digests in first-parent order (unique, max 1,024).
    pub parents: Vec<Digest>,
    /// Commit timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Producing actor and model provenance.
    pub actor: CommitActor,
    /// Workflow intent, optional task/transcript linkage, and message.
    pub intent: CommitIntent,
    /// Optional raw Git compatibility commit body (omitted completely for native Spawn commits).
    pub git_compat_body: Option<Vec<u8>>,
}

/// Pure, lossless AI provenance summary projected solely from a decoded [`CommitV2`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitProvenanceSummary {
    /// Root `spawn-tree-v1` digest.
    pub tree: Digest,
    /// Parent commit digests in first-parent order.
    pub parents: Vec<Digest>,
    /// Commit timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Producing actor identifier.
    pub agent_id: String,
    /// Optional producing execution ID.
    pub execution_id: Option<String>,
    /// Optional model label.
    pub model_label: Option<String>,
    /// Optional preset identifier.
    pub preset_id: Option<String>,
    /// Closed commit semantic kind.
    pub kind: CommitKind,
    /// Closed workflow boundary.
    pub boundary: CommitBoundary,
    /// Optional task ID.
    pub task_id: Option<i64>,
    /// Optional transcript reference.
    pub transcript_ref: Option<String>,
    /// First line of `message`.
    pub message_subject: String,
    /// Full verbatim commit message.
    pub message: String,
    /// Whether an edge-compatibility Git body trailer is attached.
    pub has_git_compat_body: bool,
}

fn validate_parents(parents: &[Digest]) -> Result<(), CasError> {
    if parents.len() > MAX_PARENTS {
        return Err(CasError::BadFormat("too many commit parents"));
    }
    let mut seen = BTreeSet::new();
    for parent in parents {
        if !seen.insert(parent) {
            return Err(CasError::BadFormat("duplicate parent digest in commit-v2"));
        }
    }
    Ok(())
}

fn validate_header_value(value: &str, empty_or_ws: &'static str) -> Result<(), CasError> {
    if value.is_empty() || value.trim() != value {
        return Err(CasError::BadFormat(empty_or_ws));
    }
    if value.len() > MAX_HEADER_VALUE_LEN {
        return Err(CasError::BadFormat("commit-v2 header value too long"));
    }
    if value.chars().any(char::is_control) {
        return Err(CasError::BadFormat(
            "commit-v2 header value contains control characters",
        ));
    }
    Ok(())
}

fn validate_optional_header(
    value: Option<&String>,
    empty_or_ws: &'static str,
) -> Result<(), CasError> {
    if let Some(inner) = value {
        validate_header_value(inner, empty_or_ws)?;
    }
    Ok(())
}

fn validate_message(message: &str) -> Result<(), CasError> {
    if message.is_empty() || message.trim().is_empty() {
        return Err(CasError::BadFormat("commit message must not be empty"));
    }
    if message
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
    {
        return Err(CasError::BadFormat(
            "commit message contains disallowed control characters",
        ));
    }
    Ok(())
}

fn validate_git_compat_body(body: &[u8]) -> Result<(), CasError> {
    if body.is_empty() {
        return Err(CasError::BadFormat(
            "git_compat_body must not be empty when present",
        ));
    }
    if body.contains(&0) {
        return Err(CasError::BadFormat(
            "git_compat_body must not contain NUL bytes",
        ));
    }
    Ok(())
}

fn parse_strict_usize(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    if bytes == b"0" {
        return Some(0);
    }
    if !matches!(bytes[0], b'1'..=b'9') || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<usize>().ok()
}

fn parse_strict_i64(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    if bytes == b"0" {
        return Some(0);
    }
    if let Some(rest) = bytes.strip_prefix(b"-") {
        if !rest.first().is_some_and(|byte| matches!(byte, b'1'..=b'9'))
            || !rest.iter().all(u8::is_ascii_digit)
        {
            return None;
        }
    } else if !matches!(bytes[0], b'1'..=b'9') || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<i64>().ok()
}

struct HeaderCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    peeked: Option<&'a [u8]>,
}

impl<'a> HeaderCursor<'a> {
    fn new(bytes: &'a [u8], pos: usize) -> Self {
        Self {
            bytes,
            pos,
            peeked: None,
        }
    }

    fn read_raw_line(&mut self) -> Result<&'a [u8], CasError> {
        let rest = self.bytes.get(self.pos..).unwrap_or(&[]);
        let rel_end = rest
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or(CasError::BadFormat("unterminated commit-v2 header line"))?;
        let line = &rest[..rel_end];
        self.pos += rel_end + 1;
        Ok(line)
    }

    fn peek_line(&mut self) -> Result<&'a [u8], CasError> {
        if self.peeked.is_none() {
            let line = self.read_raw_line()?;
            self.peeked = Some(line);
        }
        Ok(self.peeked.expect("populated above"))
    }

    fn next_line(&mut self) -> Result<&'a [u8], CasError> {
        match self.peeked.take() {
            Some(line) => Ok(line),
            None => self.read_raw_line(),
        }
    }

    fn take_optional(&mut self, prefix: &[u8]) -> Result<Option<&'a str>, CasError> {
        let line = self.peek_line()?;
        let Some(raw) = line.strip_prefix(prefix) else {
            return Ok(None);
        };
        let text = std::str::from_utf8(raw)
            .map_err(|_| CasError::BadFormat("non-utf8 commit-v2 header value"))?;
        self.peeked = None;
        Ok(Some(text))
    }

    fn take_required(&mut self, prefix: &[u8], err: &'static str) -> Result<&'a str, CasError> {
        let line = self.next_line()?;
        let raw = line.strip_prefix(prefix).ok_or(CasError::BadFormat(err))?;
        std::str::from_utf8(raw).map_err(|_| CasError::BadFormat("non-utf8 commit-v2 header value"))
    }
}

fn decode_optional_header(
    cursor: &mut HeaderCursor<'_>,
    prefix: &[u8],
    empty_or_ws: &'static str,
) -> Result<Option<String>, CasError> {
    let Some(text) = cursor.take_optional(prefix)? else {
        return Ok(None);
    };
    validate_header_value(text, empty_or_ws)?;
    Ok(Some(text.to_string()))
}

fn push_opt_header(out: &mut String, key: &str, value: Option<&String>) {
    if let Some(inner) = value {
        let _ = writeln!(out, "{key} {inner}");
    }
}

fn decode_git_compat_trailer(bytes: &[u8], pos: usize) -> Result<Option<Vec<u8>>, CasError> {
    if pos == bytes.len() {
        return Ok(None);
    }
    if bytes[pos] != b'\n' {
        return Err(CasError::BadFormat(
            "trailing bytes after commit-v2 message",
        ));
    }
    let mut trailer_cursor = HeaderCursor::new(bytes, pos + 1);
    let compat_len_str = trailer_cursor
        .take_required(b"git_compat ", "unexpected trailer after commit-v2 message")?;
    let compat_len = parse_strict_usize(compat_len_str.as_bytes())
        .ok_or(CasError::BadFormat("invalid git_compat byte length"))?;
    if compat_len == 0 {
        return Err(CasError::BadFormat(
            "git_compat_body must not be empty when present",
        ));
    }
    let body_pos = trailer_cursor.pos;
    if bytes.len().saturating_sub(body_pos) != compat_len {
        return Err(CasError::BadFormat("git_compat byte length mismatch"));
    }
    let body = &bytes[body_pos..];
    validate_git_compat_body(body)?;
    Ok(Some(body.to_vec()))
}

impl CommitV2 {
    /// Validate all structural and provenance invariants without serializing.
    fn validate(&self) -> Result<(), CasError> {
        validate_parents(&self.parents)?;
        validate_header_value(&self.actor.agent_id, "invalid commit-v2 agent_id")?;
        validate_optional_header(
            self.actor.execution_id.as_ref(),
            "invalid commit-v2 execution_id",
        )?;
        validate_optional_header(
            self.actor.model_label.as_ref(),
            "invalid commit-v2 model_label",
        )?;
        validate_optional_header(self.actor.preset_id.as_ref(), "invalid commit-v2 preset_id")?;
        validate_optional_header(
            self.intent.transcript_ref.as_ref(),
            "invalid commit-v2 transcript_ref",
        )?;
        validate_message(&self.intent.message)?;
        if let Some(body) = self.git_compat_body.as_deref() {
            validate_git_compat_body(body)?;
        }
        Ok(())
    }

    /// Encode this commit into canonical `spawn-commit-v2` wire bytes.
    pub fn encode(&self) -> Result<Vec<u8>, CasError> {
        self.validate()?;

        let mut header = String::with_capacity(256 + self.parents.len() * 80);
        header.push_str("spawn-commit-v2\n");
        let _ = writeln!(header, "tree {}", self.tree);
        for parent in &self.parents {
            let _ = writeln!(header, "parent {parent}");
        }
        let _ = writeln!(header, "at_ms {}", self.at_ms);
        let _ = writeln!(header, "agent_id {}", self.actor.agent_id);
        push_opt_header(
            &mut header,
            "execution_id",
            self.actor.execution_id.as_ref(),
        );
        push_opt_header(&mut header, "model_label", self.actor.model_label.as_ref());
        push_opt_header(&mut header, "preset_id", self.actor.preset_id.as_ref());
        let _ = writeln!(header, "kind {}", self.intent.kind);
        let _ = writeln!(header, "boundary {}", self.intent.boundary);
        if let Some(task_id) = self.intent.task_id {
            let _ = writeln!(header, "task_id {task_id}");
        }
        push_opt_header(
            &mut header,
            "transcript_ref",
            self.intent.transcript_ref.as_ref(),
        );
        let _ = writeln!(header, "message {}", self.intent.message.len());

        let compat_prefix = self
            .git_compat_body
            .as_deref()
            .map(|body| format!("\ngit_compat {}\n", body.len()));
        let compat_len = compat_prefix
            .as_ref()
            .zip(self.git_compat_body.as_deref())
            .map_or(0, |(prefix, body)| prefix.len().saturating_add(body.len()));

        let total = header
            .len()
            .saturating_add(self.intent.message.len())
            .saturating_add(compat_len);
        if total > CHUNK_MAX {
            return Err(CasError::TooLarge {
                kind: "commit",
                size: total,
                max: CHUNK_MAX,
            });
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(self.intent.message.as_bytes());
        if let (Some(prefix), Some(body)) = (compat_prefix, self.git_compat_body.as_deref()) {
            out.extend_from_slice(prefix.as_bytes());
            out.extend_from_slice(body);
        }
        Ok(out)
    }

    /// Strictly decode canonical `spawn-commit-v2` bytes (fail-closed on any violation).
    pub fn decode(bytes: &[u8]) -> Result<Self, CasError> {
        if !bytes.starts_with(COMMIT_V2_MAGIC) {
            return Err(CasError::BadFormat("missing spawn-commit-v2 header"));
        }
        if bytes.len() > CHUNK_MAX {
            return Err(CasError::TooLarge {
                kind: "commit",
                size: bytes.len(),
                max: CHUNK_MAX,
            });
        }

        let mut cursor = HeaderCursor::new(bytes, COMMIT_V2_MAGIC.len());
        let tree_str = cursor.take_required(b"tree ", "expected tree line in commit-v2")?;
        let tree = Digest::parse(tree_str)?;

        let mut parents = Vec::new();
        let mut seen_parents = BTreeSet::new();
        while let Some(parent_str) = cursor.take_optional(b"parent ")? {
            if parents.len() >= MAX_PARENTS {
                return Err(CasError::BadFormat("too many commit parents"));
            }
            let parent = Digest::parse(parent_str)?;
            if !seen_parents.insert(parent.clone()) {
                return Err(CasError::BadFormat("duplicate parent digest in commit-v2"));
            }
            parents.push(parent);
        }

        let at_ms_str = cursor.take_required(b"at_ms ", "expected at_ms line in commit-v2")?;
        let at_ms = parse_strict_i64(at_ms_str.as_bytes())
            .ok_or(CasError::BadFormat("invalid commit-v2 at_ms"))?;

        let agent_id_str =
            cursor.take_required(b"agent_id ", "expected agent_id line in commit-v2")?;
        validate_header_value(agent_id_str, "invalid commit-v2 agent_id")?;
        let execution_id = decode_optional_header(
            &mut cursor,
            b"execution_id ",
            "invalid commit-v2 execution_id",
        )?;
        let model_label = decode_optional_header(
            &mut cursor,
            b"model_label ",
            "invalid commit-v2 model_label",
        )?;
        let preset_id =
            decode_optional_header(&mut cursor, b"preset_id ", "invalid commit-v2 preset_id")?;

        let kind_str = cursor.take_required(b"kind ", "expected kind line in commit-v2")?;
        let kind = CommitKind::parse(kind_str)?;

        let boundary_str =
            cursor.take_required(b"boundary ", "expected boundary line in commit-v2")?;
        let boundary = CommitBoundary::parse(boundary_str)?;

        let task_id = match cursor.take_optional(b"task_id ")? {
            Some(raw) => Some(
                parse_strict_i64(raw.as_bytes())
                    .ok_or(CasError::BadFormat("invalid commit-v2 task_id"))?,
            ),
            None => None,
        };
        let transcript_ref = decode_optional_header(
            &mut cursor,
            b"transcript_ref ",
            "invalid commit-v2 transcript_ref",
        )?;

        let msg_len_str =
            cursor.take_required(b"message ", "expected message line in commit-v2")?;
        let msg_len = parse_strict_usize(msg_len_str.as_bytes())
            .ok_or(CasError::BadFormat("invalid commit-v2 message byte length"))?;
        if msg_len == 0 {
            return Err(CasError::BadFormat("commit message must not be empty"));
        }

        let pos = cursor.pos;
        let msg_end = pos
            .checked_add(msg_len)
            .ok_or(CasError::BadFormat("commit message length overflow"))?;
        let msg_bytes = bytes
            .get(pos..msg_end)
            .ok_or(CasError::BadFormat("truncated commit-v2 message payload"))?;
        let message = std::str::from_utf8(msg_bytes)
            .map_err(|_| CasError::BadFormat("non-utf8 commit-v2 message"))?;
        validate_message(message)?;

        let git_compat_body = decode_git_compat_trailer(bytes, msg_end)?;

        Ok(Self {
            tree,
            parents,
            at_ms,
            actor: CommitActor {
                agent_id: agent_id_str.to_string(),
                execution_id,
                model_label,
                preset_id,
            },
            intent: CommitIntent {
                kind,
                boundary,
                task_id,
                transcript_ref,
                message: message.to_string(),
            },
            git_compat_body,
        })
    }

    /// Canonical SHA-256 content address of this encoded commit object.
    pub fn digest(&self) -> Result<Digest, CasError> {
        Ok(sha256_of(&self.encode()?))
    }

    /// Lossless pure projection of code identity and AI lineage fields from this commit.
    pub fn provenance_summary(&self) -> CommitProvenanceSummary {
        let message_subject = self
            .intent
            .message
            .split('\n')
            .next()
            .unwrap_or("")
            .to_string();
        CommitProvenanceSummary {
            tree: self.tree.clone(),
            parents: self.parents.clone(),
            at_ms: self.at_ms,
            agent_id: self.actor.agent_id.clone(),
            execution_id: self.actor.execution_id.clone(),
            model_label: self.actor.model_label.clone(),
            preset_id: self.actor.preset_id.clone(),
            kind: self.intent.kind,
            boundary: self.intent.boundary,
            task_id: self.intent.task_id,
            transcript_ref: self.intent.transcript_ref.clone(),
            message_subject,
            message: self.intent.message.clone(),
            has_git_compat_body: self.git_compat_body.is_some(),
        }
    }
}

impl Store {
    /// Store a canonical `spawn-commit-v2` commit object in [`NS_TREE`],
    /// returning its SHA-256 content address.
    pub fn put_commit_v2(&self, commit: &CommitV2) -> Result<Digest, CasError> {
        let bytes = commit.encode()?;
        let digest = sha256_of(&bytes);
        self.put_hashed_bytes(NS_TREE, &bytes, &digest)?;
        Ok(digest)
    }

    /// Read, SHA-256 verify, and decode either a legacy `spawn-snapshot-v1` or a
    /// `spawn-commit-v2` object at `digest`.
    pub fn get_commit_object(&self, digest: &Digest) -> Result<CommitObject, CasError> {
        let bytes = self.read(digest)?;
        CommitObject::decode(&bytes)
    }
}
