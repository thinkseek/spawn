//! D6 Phase 4 Step 3 (`#113` Comment #5): pure-Rust three-way Merkle tree merge,
//! single merge-base guard, `.gitattributes` parser, linear-space Myers diff +
//! `diff3` three-way text merge, exact-content rename resolution, `Spawn-Head`
//! merge snapshot builder, and shadow gate divergence classifier.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::error::Error;
use std::fmt::{self, Write as _};

use spawn_cas::{
    Digest, GIT_POINTER_THRESHOLD, GitOid, Snapshot, Tree, TreeEntry, TreeEntryKind,
    empty_tree_digest,
};

use crate::cas_merkle::{CasMerkleStore, D6StoreError, format_commit_body};
use crate::git::{CONFLICT_MARKER_SIZE, LARGE_FILE_TRAILER};
use crate::tree_path::TreePath;

/// Errors produced when validating snapshot ancestry for a 3-way D6 merge.
#[derive(Debug)]
pub enum MergeBaseError {
    /// Two snapshots share no common ancestor in the snapshot DAG.
    NoCommonAncestor {
        /// First (ours/main) snapshot digest.
        ours: Digest,
        /// Second (theirs/branch) snapshot digest.
        theirs: Digest,
    },
    /// Criss-cross DAG yielded multiple lowest common ancestors (explicitly
    /// rejected in `#113` Comment #5 rather than synthesizing recursive trees).
    MultipleMergeBases {
        /// First snapshot digest.
        ours: Digest,
        /// Second snapshot digest.
        theirs: Digest,
        /// Lowest common ancestors in canonical digest order.
        bases: Vec<Digest>,
    },
    /// Underlying CAS / snapshot decoding / projection failure.
    Store(D6StoreError),
}

impl fmt::Display for MergeBaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCommonAncestor { ours, theirs } => {
                write!(f, "no common merge-base between {ours} and {theirs}")
            }
            Self::MultipleMergeBases {
                ours,
                theirs,
                bases,
            } => write!(
                f,
                "refusing criss-cross merge between {ours} and {theirs}: {} merge-bases ({})",
                bases.len(),
                bases
                    .iter()
                    .map(Digest::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Store(err) => write!(f, "{err}"),
        }
    }
}

impl Error for MergeBaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            _ => None,
        }
    }
}

impl From<D6StoreError> for MergeBaseError {
    fn from(err: D6StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<spawn_cas::CasError> for MergeBaseError {
    fn from(err: spawn_cas::CasError) -> Self {
        Self::Store(D6StoreError::Cas(err))
    }
}

/// Compute all lowest common ancestors (merge-bases) of `ours` and `theirs` in
/// the persistent `spawn-snapshot-v1` DAG stored inside `store`.
pub fn find_merge_bases(
    store: &CasMerkleStore,
    ours: &Digest,
    theirs: &Digest,
) -> Result<Vec<Digest>, D6StoreError> {
    let mut parents_cache: HashMap<Digest, Vec<Digest>> = HashMap::new();
    let anc_ours = collect_ancestors(store, ours, &mut parents_cache)?;
    let anc_theirs = collect_ancestors(store, theirs, &mut parents_cache)?;

    let common: BTreeSet<Digest> = anc_ours.intersection(&anc_theirs).cloned().collect();
    if common.len() <= 1 {
        return Ok(common.into_iter().collect());
    }

    // Any common ancestor reachable as a strict ancestor of another common
    // ancestor is dominated and not a lowest common ancestor.
    let mut dominated: BTreeSet<Digest> = BTreeSet::new();
    let mut stack: Vec<Digest> = Vec::new();
    for cand in &common {
        if let Some(parents) = parents_cache.get(cand) {
            stack.extend(parents.iter().cloned());
        }
    }
    while let Some(node) = stack.pop() {
        if !dominated.insert(node.clone()) {
            continue;
        }
        if let Some(parents) = parents_cache.get(&node) {
            stack.extend(parents.iter().cloned());
        }
    }

    Ok(common.difference(&dominated).cloned().collect())
}

fn collect_ancestors(
    store: &CasMerkleStore,
    start: &Digest,
    parents_cache: &mut HashMap<Digest, Vec<Digest>>,
) -> Result<BTreeSet<Digest>, D6StoreError> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    seen.insert(start.clone());
    queue.push_back(start.clone());

    while let Some(id) = queue.pop_front() {
        let parents = if let Some(cached) = parents_cache.get(&id) {
            cached.clone()
        } else {
            let snap = store.cas().read_merkle_snapshot(&id)?;
            parents_cache.insert(id.clone(), snap.parents.clone());
            snap.parents
        };
        for p in parents {
            if seen.insert(p.clone()) {
                queue.push_back(p);
            }
        }
    }
    Ok(seen)
}

/// Enforce the D6 single merge-base invariant: accept iff `bases.len() == 1`.
pub fn validate_single_merge_base(
    bases: &[Digest],
    ours: &Digest,
    theirs: &Digest,
) -> Result<Digest, MergeBaseError> {
    match bases {
        [] => Err(MergeBaseError::NoCommonAncestor {
            ours: ours.clone(),
            theirs: theirs.clone(),
        }),
        [single] => Ok(single.clone()),
        many => {
            let mut sorted = many.to_vec();
            sorted.sort();
            sorted.dedup();
            if sorted.len() == 1 {
                Ok(sorted.into_iter().next().expect("non-empty"))
            } else {
                Err(MergeBaseError::MultipleMergeBases {
                    ours: ours.clone(),
                    theirs: theirs.clone(),
                    bases: sorted,
                })
            }
        }
    }
}

/// Discover merge-bases for `(ours, theirs)` and fail closed unless there is
/// strictly one lowest common ancestor.
pub fn find_single_merge_base(
    store: &CasMerkleStore,
    ours: &Digest,
    theirs: &Digest,
) -> Result<Digest, MergeBaseError> {
    let bases = find_merge_bases(store, ours, theirs)?;
    validate_single_merge_base(&bases, ours, theirs)
}

/// Merge driver resolved from `.gitattributes` for a file path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MergeDriver {
    /// Standard 3-way line merge (`diff3` conflict markers on overlap).
    #[default]
    Text,
    /// Binary file (`-merge`, `merge=binary`, or `binary` attribute).
    Binary,
    /// Union 3-way merge (`merge=union`, concatenate ours then theirs cleanly).
    Union,
    /// External custom driver unsupported in pure-Rust CAS merge (fail-closed).
    Custom(String),
}

/// Resolved `text` attribute state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAttr {
    /// Attribute unspecified.
    #[default]
    Unspecified,
    /// Explicitly `text`.
    Set,
    /// `text=auto`.
    Auto,
    /// Explicitly `-text` or implied by `binary`.
    Unset,
}

/// Resolved `eol` attribute state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EolAttr {
    /// Attribute unspecified.
    #[default]
    Unspecified,
    /// `eol=lf`.
    Lf,
    /// `eol=crlf`.
    Crlf,
}

/// Resolved `.gitattributes` settings governing three-way merge for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAttributes {
    /// Selected merge driver (`text`, `binary`, `union`, or unsupported custom).
    pub merge: MergeDriver,
    /// Whether the `binary` macro was set and not subsequently overridden by text/merge.
    pub binary: bool,
    /// Resolved `text` attribute.
    pub text: TextAttr,
    /// Resolved `eol` attribute.
    pub eol: EolAttr,
    /// Conflict marker length (`conflict-marker-size`, default `7`).
    pub conflict_marker_size: usize,
}

impl Default for PathAttributes {
    fn default() -> Self {
        Self {
            merge: MergeDriver::Text,
            binary: false,
            text: TextAttr::Unspecified,
            eol: EolAttr::Unspecified,
            conflict_marker_size: CONFLICT_MARKER_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
enum AttrState {
    Set,
    Unset,
    Unspecified,
    Value(String),
}

#[derive(Debug, Clone)]
struct AttrRule {
    anchored: bool,
    segments: Vec<Vec<u8>>,
    assignments: Vec<(String, AttrState)>,
}

#[derive(Debug, Clone)]
struct AttrLayer {
    base_dir: Vec<u8>,
    rules: Vec<AttrRule>,
}

/// Stacked `.gitattributes` evaluator supporting `[attr]<macro>` definitions,
/// built-in `binary` macro, quoted patterns, and directory-scoped precedence.
#[derive(Debug, Clone, Default)]
pub struct GitAttributes {
    macros: HashMap<String, Vec<(String, AttrState)>>,
    layers: Vec<AttrLayer>,
    override_layers: Vec<AttrLayer>,
}

impl GitAttributes {
    /// Parse a single root-level `.gitattributes` text together with optional override lines
    /// (ordered highest precedence, akin to `.git/info/attributes`).
    pub fn from_root_str(
        root_attributes: &str,
        extra_attributes: &[String],
    ) -> Result<Self, D6StoreError> {
        let mut attrs = Self::default();
        if !root_attributes.is_empty() {
            attrs.push_root_bytes(root_attributes.as_bytes())?;
        }
        attrs.set_override_lines(extra_attributes)?;
        Ok(attrs)
    }

    fn set_override_lines(&mut self, lines: &[String]) -> Result<(), D6StoreError> {
        if lines.is_empty() {
            return Ok(());
        }
        let joined = lines.join("\n");
        let layer = self.parse_layer(b"", joined.as_bytes(), true)?;
        self.override_layers.push(layer);
        Ok(())
    }

    fn push_root_bytes(&mut self, bytes: &[u8]) -> Result<(), D6StoreError> {
        let layer = self.parse_layer(b"", bytes, true)?;
        self.layers.push(layer);
        Ok(())
    }

    fn push_dir_bytes(&mut self, base_dir: &[u8], bytes: &[u8]) -> Result<(), D6StoreError> {
        let layer = self.parse_layer(base_dir, bytes, base_dir.is_empty())?;
        self.layers.push(layer);
        Ok(())
    }

    fn pop_layer(&mut self) {
        let _ = self.layers.pop();
    }

    fn parse_layer(
        &mut self,
        base_dir: &[u8],
        bytes: &[u8],
        allow_macros: bool,
    ) -> Result<AttrLayer, D6StoreError> {
        let mut rules = Vec::new();
        for raw_line in bytes.split(|&b| b == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            let trimmed_start = trim_leading_ascii_ws(line);
            if trimmed_start.is_empty() || trimmed_start[0] == b'#' {
                continue;
            }
            let Some((raw_pat, rest)) = parse_attr_pattern(trimmed_start)? else {
                continue;
            };
            let tokens = split_ascii_ws_strs(rest)?;
            if tokens.is_empty() {
                continue;
            }
            let parsed_assigns = tokens
                .into_iter()
                .map(|tok| parse_attr_token(&tok))
                .collect::<Vec<_>>();

            if allow_macros && let Some(macro_name) = raw_pat.strip_prefix(b"[attr]") {
                let name = String::from_utf8(macro_name.to_vec()).map_err(|_| {
                    D6StoreError::InvalidInput("non-utf8 gitattributes macro name".to_string())
                })?;
                if !name.is_empty() {
                    self.macros.insert(name, parsed_assigns);
                }
                continue;
            }

            // `man gitattributes`: negative patterns and trailing-slash directory patterns are ignored.
            if raw_pat.starts_with(b"!") || raw_pat.ends_with(b"/") {
                continue;
            }
            let mut pat = raw_pat;
            if let Some(escaped) = pat.strip_prefix(b"\\!") {
                let mut unescaped = Vec::with_capacity(escaped.len() + 1);
                unescaped.push(b'!');
                unescaped.extend_from_slice(escaped);
                pat = unescaped;
            }
            let anchored = if let Some(stripped) = pat.strip_prefix(b"/") {
                pat = stripped.to_vec();
                true
            } else {
                pat.contains(&b'/')
            };
            if pat.is_empty() {
                continue;
            }
            let segments: Vec<Vec<u8>> = pat.split(|&b| b == b'/').map(<[u8]>::to_vec).collect();
            rules.push(AttrRule {
                anchored,
                segments,
                assignments: parsed_assigns,
            });
        }
        Ok(AttrLayer {
            base_dir: base_dir.to_vec(),
            rules,
        })
    }

    /// Resolve merged `.gitattributes` properties for `rel_path`.
    pub fn resolve_path(&self, rel_path: &[u8]) -> PathAttributes {
        let mut raw_map: HashMap<String, AttrState> = HashMap::new();
        for layer in self.layers.iter().chain(self.override_layers.iter()) {
            let scoped = if layer.base_dir.is_empty() {
                rel_path
            } else if let Some(after) = rel_path.strip_prefix(layer.base_dir.as_slice())
                && let Some(rest) = after.strip_prefix(b"/")
            {
                rest
            } else {
                continue;
            };
            for rule in &layer.rules {
                if rule.matches(scoped) {
                    for (name, state) in &rule.assignments {
                        self.apply_assignment(name, state, &mut raw_map, 0);
                    }
                }
            }
        }

        interpret_raw_attributes(&raw_map)
    }

    fn apply_assignment(
        &self,
        name: &str,
        state: &AttrState,
        raw_map: &mut HashMap<String, AttrState>,
        depth: usize,
    ) {
        if depth > 8 {
            return;
        }
        raw_map.insert(name.to_string(), state.clone());
        match (name, state) {
            ("binary", AttrState::Set) => {
                self.apply_assignment("diff", &AttrState::Unset, raw_map, depth + 1);
                self.apply_assignment("merge", &AttrState::Unset, raw_map, depth + 1);
                self.apply_assignment("text", &AttrState::Unset, raw_map, depth + 1);
            }
            ("binary", AttrState::Unset) => {
                self.apply_assignment("diff", &AttrState::Set, raw_map, depth + 1);
                self.apply_assignment("merge", &AttrState::Set, raw_map, depth + 1);
                self.apply_assignment("text", &AttrState::Set, raw_map, depth + 1);
            }
            ("binary", AttrState::Unspecified) => {
                self.apply_assignment("diff", &AttrState::Unspecified, raw_map, depth + 1);
                self.apply_assignment("merge", &AttrState::Unspecified, raw_map, depth + 1);
                self.apply_assignment("text", &AttrState::Unspecified, raw_map, depth + 1);
            }
            _ => {
                if let Some(expanded) = self.macros.get(name) {
                    for (sub_name, sub_state) in expanded {
                        let effective = match state {
                            AttrState::Set | AttrState::Value(_) => sub_state.clone(),
                            AttrState::Unset => match sub_state {
                                AttrState::Set => AttrState::Unset,
                                AttrState::Unset => AttrState::Set,
                                other => other.clone(),
                            },
                            AttrState::Unspecified => AttrState::Unspecified,
                        };
                        self.apply_assignment(sub_name, &effective, raw_map, depth + 1);
                    }
                }
            }
        }
    }
}

fn interpret_raw_attributes(raw_map: &HashMap<String, AttrState>) -> PathAttributes {
    let mut out = PathAttributes::default();
    if matches!(raw_map.get("binary"), Some(AttrState::Set)) {
        out.binary = true;
    }
    if let Some(state) = raw_map.get("text") {
        out.text = match state {
            AttrState::Set => TextAttr::Set,
            AttrState::Unset => TextAttr::Unset,
            AttrState::Value(v) if v == "auto" => TextAttr::Auto,
            _ => TextAttr::Unspecified,
        };
    }
    if let Some(state) = raw_map.get("eol") {
        out.eol = match state {
            AttrState::Value(v) if v == "lf" => EolAttr::Lf,
            AttrState::Value(v) if v == "crlf" => EolAttr::Crlf,
            _ => EolAttr::Unspecified,
        };
    }
    if let Some(state) = raw_map.get("merge") {
        out.merge = match state {
            AttrState::Unset => MergeDriver::Binary,
            AttrState::Set | AttrState::Unspecified => MergeDriver::Text,
            AttrState::Value(v) if v == "text" => MergeDriver::Text,
            AttrState::Value(v) if v == "binary" => MergeDriver::Binary,
            AttrState::Value(v) if v == "union" => MergeDriver::Union,
            AttrState::Value(other) => MergeDriver::Custom(other.clone()),
        };
    }
    if let Some(AttrState::Value(val)) = raw_map.get("conflict-marker-size")
        && let Ok(n) = val.parse::<usize>()
        && (1..=1024).contains(&n)
    {
        out.conflict_marker_size = n;
    }
    out
}

impl AttrRule {
    fn matches(&self, scoped_rel: &[u8]) -> bool {
        if !self.anchored && self.segments.len() == 1 {
            let basename = scoped_rel
                .rsplit(|&b| b == b'/')
                .next()
                .unwrap_or(scoped_rel);
            return match_glob_segment(&self.segments[0], basename);
        }
        let path_segs: Vec<&[u8]> = scoped_rel.split(|&b| b == b'/').collect();
        match_glob_segments(&self.segments, &path_segs)
    }
}

fn trim_leading_ascii_ws(mut line: &[u8]) -> &[u8] {
    while let Some((&b, rest)) = line.split_first() {
        if matches!(b, b' ' | b'\t') {
            line = rest;
        } else {
            break;
        }
    }
    line
}

type ParsedAttrPattern<'a> = (Vec<u8>, &'a [u8]);

fn parse_attr_pattern(line: &[u8]) -> Result<Option<ParsedAttrPattern<'_>>, D6StoreError> {
    if line.is_empty() {
        return Ok(None);
    }
    if line[0] == b'"' {
        let mut pat = Vec::new();
        let mut i = 1;
        while i < line.len() {
            match line[i] {
                b'"' => return Ok(Some((pat, &line[i + 1..]))),
                b'\\' => {
                    if i + 1 >= line.len() {
                        return Err(D6StoreError::InvalidInput(
                            "dangling escape in .gitattributes pattern".to_string(),
                        ));
                    }
                    let esc = match line[i + 1] {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        other => other,
                    };
                    pat.push(esc);
                    i += 2;
                }
                b => {
                    pat.push(b);
                    i += 1;
                }
            }
        }
        return Err(D6StoreError::InvalidInput(
            "unclosed quote in .gitattributes pattern".to_string(),
        ));
    }

    let mut pat = Vec::new();
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' | b'\t' => return Ok(Some((pat, &line[i..]))),
            b'\\' if i + 1 < line.len() => {
                pat.push(line[i + 1]);
                i += 2;
            }
            b => {
                pat.push(b);
                i += 1;
            }
        }
    }
    Ok(Some((pat, &[])))
}

fn split_ascii_ws_strs(bytes: &[u8]) -> Result<Vec<String>, D6StoreError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| D6StoreError::InvalidInput("non-utf8 attribute tokens".to_string()))?;
    Ok(text.split_ascii_whitespace().map(str::to_string).collect())
}

fn parse_attr_token(tok: &str) -> (String, AttrState) {
    if let Some(rest) = tok.strip_prefix('-') {
        (rest.to_string(), AttrState::Unset)
    } else if let Some(rest) = tok.strip_prefix('!') {
        (rest.to_string(), AttrState::Unspecified)
    } else if let Some((name, val)) = tok.split_once('=') {
        (name.to_string(), AttrState::Value(val.to_string()))
    } else {
        (tok.to_string(), AttrState::Set)
    }
}

fn match_glob_segments(pat: &[Vec<u8>], path: &[&[u8]]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0].as_slice() == b"**" {
        if pat.len() == 1 {
            return !path.is_empty();
        }
        for split in 0..=path.len() {
            if match_glob_segments(&pat[1..], &path[split..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    match_glob_segment(&pat[0], path[0]) && match_glob_segments(&pat[1..], &path[1..])
}

fn match_glob_segment(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut wildcard_pat_idx: Option<usize> = None;
    let mut backtrack_text_idx = 0;

    while ti < text.len() {
        if pi < pat.len() && pat[pi] == b'*' {
            wildcard_pat_idx = Some(pi);
            pi += 1;
            backtrack_text_idx = ti;
        } else if let Some(next_pi) = match_glob_token(pat, pi, text[ti]) {
            pi = next_pi;
            ti += 1;
        } else if let Some(sp) = wildcard_pat_idx {
            pi = sp + 1;
            backtrack_text_idx += 1;
            ti = backtrack_text_idx;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

fn match_glob_token(pat: &[u8], pi: usize, ch: u8) -> Option<usize> {
    if pi >= pat.len() {
        return None;
    }
    match pat[pi] {
        b'?' => Some(pi + 1),
        b'\\' => (pi + 1 < pat.len() && pat[pi + 1] == ch).then_some(pi + 2),
        b'[' => match_glob_char_class(pat, pi, ch),
        other => (other == ch).then_some(pi + 1),
    }
}

fn match_glob_char_class(pat: &[u8], start: usize, ch: u8) -> Option<usize> {
    let mut i = start + 1;
    let mut negated = false;
    if i < pat.len() && matches!(pat[i], b'!' | b'^') {
        negated = true;
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            let ok = if negated { !hit } else { hit };
            return ok.then_some(i + 1);
        }
        first = false;
        let lo = pat[i];
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let hi = pat[i + 2];
            if lo <= ch && ch <= hi {
                hit = true;
            }
            i += 3;
        } else {
            if ch == lo {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

/// Labels written onto `diff3` conflict markers (`<<<<<<< <ours>`, `||||||| <base>`,
/// `>>>>>>> <theirs>`). Empty strings omit labels (bare markers).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConflictLabels {
    /// First side label (mainline / ours).
    pub ours: String,
    /// Common ancestor label (base).
    pub base: String,
    /// Second side label (branch / theirs).
    pub theirs: String,
}

/// Result of three-way text merge over one file's raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMergeResult {
    /// Merged byte output (contains `diff3` markers iff `conflicted == true`).
    pub bytes: Vec<u8>,
    /// Whether any overlapping/divergent conflict occurred.
    pub conflicted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditHunk {
    a_start: usize,
    a_end: usize,
    b_start: usize,
    b_end: usize,
}

/// Whether `bytes` begins with NUL within Git's standard `buffer_is_binary` 8000-byte probe window.
pub fn is_binary_buffer(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8000);
    bytes[..probe_len].contains(&0)
}

/// Split raw bytes into line slices preserving each line's trailing `b'\n'`.
fn split_lines_keep_newline(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(&bytes[start..=idx]);
            start = idx + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

/// Pure-Rust deterministic three-way line merge producing exact `diff3` markers
/// (supporting `MergeDriver::Union` and custom `conflict-marker-size`).
#[expect(clippy::too_many_lines)]
pub fn merge_text_3way(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    driver: &MergeDriver,
    marker_size: usize,
    labels: &ConflictLabels,
) -> TextMergeResult {
    if ours == theirs {
        return TextMergeResult {
            bytes: ours.to_vec(),
            conflicted: false,
        };
    }
    if base == ours {
        return TextMergeResult {
            bytes: theirs.to_vec(),
            conflicted: false,
        };
    }
    if base == theirs {
        return TextMergeResult {
            bytes: ours.to_vec(),
            conflicted: false,
        };
    }

    let base_lines = split_lines_keep_newline(base);
    let ours_lines = split_lines_keep_newline(ours);
    let theirs_lines = split_lines_keep_newline(theirs);

    let mut intern: HashMap<&[u8], u32> = HashMap::with_capacity(base_lines.len() + 64);
    let base_ids = intern_lines(&base_lines, &mut intern);
    let ours_ids = intern_lines(&ours_lines, &mut intern);
    let theirs_ids = intern_lines(&theirs_lines, &mut intern);

    let edits_ours = myers_diff_hunks(&base_ids, &ours_ids);
    let edits_theirs = myers_diff_hunks(&base_ids, &theirs_ids);

    let marker_len = marker_size.clamp(1, 1024);
    let mut out = Vec::with_capacity(ours.len().max(theirs.len()) + 64);
    let mut conflicted = false;

    let mut oi = 0_usize;
    let mut ti = 0_usize;
    let mut base_pos = 0_usize;

    while oi < edits_ours.len() || ti < edits_theirs.len() {
        let next_start = match (edits_ours.get(oi), edits_theirs.get(ti)) {
            (Some(a), Some(b)) => a.a_start.min(b.a_start),
            (Some(a), None) => a.a_start,
            (None, Some(b)) => b.a_start,
            (None, None) => break,
        };

        for &line in &base_lines[base_pos..next_start] {
            out.extend_from_slice(line);
        }

        // Gather all touching or overlapping hunks on `base` into one merge region.
        let cluster_start = next_start;
        let mut cluster_end = next_start;
        let mut ours_in_cluster = false;
        let mut theirs_in_cluster = false;
        let oi_start = oi;
        let ti_start = ti;

        loop {
            let mut progressed = false;
            if let Some(eo) = edits_ours.get(oi)
                && eo.a_start <= cluster_end
            {
                cluster_end = cluster_end.max(eo.a_end);
                ours_in_cluster = true;
                oi += 1;
                progressed = true;
            }
            if let Some(et) = edits_theirs.get(ti)
                && et.a_start <= cluster_end
            {
                cluster_end = cluster_end.max(et.a_end);
                theirs_in_cluster = true;
                ti += 1;
                progressed = true;
            }
            if !progressed {
                break;
            }
        }

        let ours_chunk = reconstruct_side(
            &base_lines,
            &ours_lines,
            cluster_start,
            cluster_end,
            &edits_ours[oi_start..oi],
        );
        let theirs_chunk = reconstruct_side(
            &base_lines,
            &theirs_lines,
            cluster_start,
            cluster_end,
            &edits_theirs[ti_start..ti],
        );

        if !ours_in_cluster {
            emit_lines(&mut out, &theirs_chunk);
        } else if !theirs_in_cluster || lines_equal(&ours_chunk, &theirs_chunk) {
            emit_lines(&mut out, &ours_chunk);
        } else if *driver == MergeDriver::Union {
            emit_lines(&mut out, &ours_chunk);
            emit_lines(&mut out, &theirs_chunk);
        } else {
            conflicted = true;
            let base_chunk = &base_lines[cluster_start..cluster_end];
            emit_diff3_conflict(
                &mut out,
                marker_len,
                labels,
                &ours_chunk,
                base_chunk,
                &theirs_chunk,
            );
        }

        base_pos = cluster_end;
    }

    for &line in &base_lines[base_pos..] {
        out.extend_from_slice(line);
    }

    TextMergeResult {
        bytes: out,
        conflicted,
    }
}

fn intern_lines<'a>(seq: &[&'a [u8]], intern: &mut HashMap<&'a [u8], u32>) -> Vec<u32> {
    let mut ids = Vec::with_capacity(seq.len());
    for &line in seq {
        let next_id = u32::try_from(intern.len()).expect("line count fits u32");
        let id = *intern.entry(line).or_insert(next_id);
        ids.push(id);
    }
    ids
}

fn reconstruct_side<'a>(
    base_lines: &[&'a [u8]],
    side_lines: &[&'a [u8]],
    span_start: usize,
    span_end: usize,
    hunks: &[EditHunk],
) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut cursor = span_start;
    for h in hunks {
        if h.a_start > cursor {
            out.extend_from_slice(&base_lines[cursor..h.a_start]);
        }
        out.extend_from_slice(&side_lines[h.b_start..h.b_end]);
        cursor = h.a_end;
    }
    if span_end > cursor {
        out.extend_from_slice(&base_lines[cursor..span_end]);
    }
    out
}

fn lines_equal(a: &[&[u8]], b: &[&[u8]]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

fn emit_lines(out: &mut Vec<u8>, lines: &[&[u8]]) {
    for &line in lines {
        out.extend_from_slice(line);
    }
}

fn emit_section_with_trailing_nl(out: &mut Vec<u8>, lines: &[&[u8]]) {
    if lines.is_empty() {
        return;
    }
    for &line in lines {
        out.extend_from_slice(line);
    }
    if out.last().copied() != Some(b'\n') {
        out.push(b'\n');
    }
}

fn write_marker_line(out: &mut Vec<u8>, ch: u8, count: usize, label: &str) {
    out.resize(out.len() + count, ch);
    if !label.is_empty() {
        out.push(b' ');
        out.extend_from_slice(label.as_bytes());
    }
    out.push(b'\n');
}

fn emit_diff3_conflict(
    out: &mut Vec<u8>,
    marker_len: usize,
    labels: &ConflictLabels,
    ours_lines: &[&[u8]],
    base_lines: &[&[u8]],
    theirs_lines: &[&[u8]],
) {
    write_marker_line(out, b'<', marker_len, &labels.ours);
    emit_section_with_trailing_nl(out, ours_lines);
    write_marker_line(out, b'|', marker_len, &labels.base);
    emit_section_with_trailing_nl(out, base_lines);
    write_marker_line(out, b'=', marker_len, "");
    emit_section_with_trailing_nl(out, theirs_lines);
    write_marker_line(out, b'>', marker_len, &labels.theirs);
}

/// Divide-and-conquer linear-space Myers diff with downward `xdl_change_compact`
/// hunk compaction.
fn myers_diff_hunks(a: &[u32], b: &[u32]) -> Vec<EditHunk> {
    let mut raw = Vec::new();
    myers_recurse(a, b, 0, a.len(), 0, b.len(), &mut raw);
    coalesce_and_compact_hunks(a, b, raw)
}

fn myers_recurse(
    a: &[u32],
    b: &[u32],
    mut a_lo: usize,
    mut a_hi: usize,
    mut b_lo: usize,
    mut b_hi: usize,
    out: &mut Vec<EditHunk>,
) {
    while a_lo < a_hi && b_lo < b_hi && a[a_lo] == b[b_lo] {
        a_lo += 1;
        b_lo += 1;
    }
    while a_hi > a_lo && b_hi > b_lo && a[a_hi - 1] == b[b_hi - 1] {
        a_hi -= 1;
        b_hi -= 1;
    }

    let n = a_hi - a_lo;
    let m = b_hi - b_lo;
    if n == 0 {
        if m > 0 {
            out.push(EditHunk {
                a_start: a_lo,
                a_end: a_lo,
                b_start: b_lo,
                b_end: b_hi,
            });
        }
        return;
    }
    if m == 0 {
        out.push(EditHunk {
            a_start: a_lo,
            a_end: a_hi,
            b_start: b_lo,
            b_end: b_lo,
        });
        return;
    }

    let (x_start, y_start, x_end, y_end) = find_middle_snake(&a[a_lo..a_hi], &b[b_lo..b_hi]);
    myers_recurse(a, b, a_lo, a_lo + x_start, b_lo, b_lo + y_start, out);
    myers_recurse(a, b, a_lo + x_end, a_hi, b_lo + y_end, b_hi, out);
}

#[expect(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn find_middle_snake(lhs: &[u32], rhs: &[u32]) -> (usize, usize, usize, usize) {
    let len_lhs = lhs.len();
    let len_rhs = rhs.len();
    let delta = len_lhs as isize - len_rhs as isize;
    let odd = (delta & 1) != 0;
    let max_d = (len_lhs + len_rhs).div_ceil(2);
    let offset = (max_d + 2) as isize;
    let vec_len = max_d * 2 + 5;
    let mut vf = vec![0_isize; vec_len];
    let mut vb = vec![0_isize; vec_len];

    // Forward/backward `d = 0` seeds (note lhs[0] != rhs[0] and lhs[len-1] != rhs[len-1],
    // so both `d = 0` snakes have zero length).
    vf[offset as usize] = 0;
    vb[offset as usize] = 0;

    for step in 1..=max_d {
        let step_isize = step as isize;

        // Forward pass
        let mut diag_k = -step_isize;
        while diag_k <= step_isize {
            let idx = (diag_k + offset) as usize;
            let down = diag_k == -step_isize || (diag_k != step_isize && vf[idx - 1] < vf[idx + 1]);
            let mut pos_x = if down { vf[idx + 1] } else { vf[idx - 1] + 1 };
            let mut pos_y = pos_x - diag_k;
            let start_x = pos_x;
            let start_y = pos_y;

            while pos_x >= 0
                && pos_y >= 0
                && (pos_x as usize) < len_lhs
                && (pos_y as usize) < len_rhs
                && lhs[pos_x as usize] == rhs[pos_y as usize]
            {
                pos_x += 1;
                pos_y += 1;
            }
            vf[idx] = pos_x;

            let back_k = delta - diag_k;
            if odd && back_k >= -(step_isize - 1) && back_k <= (step_isize - 1) {
                let back_x = len_lhs as isize - vb[(back_k + offset) as usize];
                if pos_x >= back_x {
                    return (
                        start_x.clamp(0, len_lhs as isize) as usize,
                        start_y.clamp(0, len_rhs as isize) as usize,
                        pos_x.clamp(0, len_lhs as isize) as usize,
                        pos_y.clamp(0, len_rhs as isize) as usize,
                    );
                }
            }
            diag_k += 2;
        }

        // Backward pass using distance `(rev_u, rev_v)` from `(len_lhs, len_rhs)`
        let mut diag_c = -step_isize;
        while diag_c <= step_isize {
            let idx = (diag_c + offset) as usize;
            let down = diag_c == -step_isize || (diag_c != step_isize && vb[idx - 1] < vb[idx + 1]);
            let mut rev_u = if down { vb[idx + 1] } else { vb[idx - 1] + 1 };
            let mut rev_v = rev_u - diag_c;
            let u_start = rev_u;
            let v_start = rev_v;

            while rev_u >= 0
                && rev_v >= 0
                && (rev_u as usize) < len_lhs
                && (rev_v as usize) < len_rhs
                && lhs[len_lhs - 1 - (rev_u as usize)] == rhs[len_rhs - 1 - (rev_v as usize)]
            {
                rev_u += 1;
                rev_v += 1;
            }
            vb[idx] = rev_u;

            let fwd_k = delta - diag_c;
            if !odd && fwd_k >= -step_isize && fwd_k <= step_isize {
                let fwd_x = vf[(fwd_k + offset) as usize];
                let back_x = len_lhs as isize - rev_u;
                if fwd_x >= back_x {
                    return (
                        (len_lhs as isize - rev_u).clamp(0, len_lhs as isize) as usize,
                        (len_rhs as isize - rev_v).clamp(0, len_rhs as isize) as usize,
                        (len_lhs as isize - u_start).clamp(0, len_lhs as isize) as usize,
                        (len_rhs as isize - v_start).clamp(0, len_rhs as isize) as usize,
                    );
                }
            }
            diag_c += 2;
        }
    }

    (0, 0, len_lhs, len_rhs)
}

fn coalesce_and_compact_hunks(a: &[u32], b: &[u32], raw: Vec<EditHunk>) -> Vec<EditHunk> {
    let mut hunks = coalesce_touching(raw);
    // Slide pure insertions and deletions upward then downward (`xdl_change_compact` style),
    // coalescing after each sweep until the hunk count stabilizes.
    loop {
        let prev_len = hunks.len();
        shift_pure_hunks_up(a, b, &mut hunks);
        hunks = coalesce_touching(hunks);
        shift_pure_hunks_down(a, b, &mut hunks);
        hunks = coalesce_touching(hunks);
        if hunks.len() >= prev_len {
            break;
        }
    }
    hunks
}

fn shift_pure_hunks_up(a: &[u32], b: &[u32], hunks: &mut [EditHunk]) {
    for idx in 0..hunks.len() {
        let (a_min, b_min) = if idx == 0 {
            (0, 0)
        } else {
            (hunks[idx - 1].a_end, hunks[idx - 1].b_end)
        };
        let h = &mut hunks[idx];
        if h.a_start == h.a_end && h.b_start < h.b_end {
            // Pure insertion: retreat while preceding unchanged line matches last inserted line.
            while h.a_start > a_min && h.b_start > b_min && a[h.a_start - 1] == b[h.b_end - 1] {
                h.a_start -= 1;
                h.a_end -= 1;
                h.b_start -= 1;
                h.b_end -= 1;
            }
        } else if h.b_start == h.b_end && h.a_start < h.a_end {
            // Pure deletion: retreat while preceding unchanged line matches last deleted line.
            while h.a_start > a_min && h.b_start > b_min && a[h.a_start - 1] == a[h.a_end - 1] {
                h.a_start -= 1;
                h.a_end -= 1;
                h.b_start -= 1;
                h.b_end -= 1;
            }
        }
    }
}

fn shift_pure_hunks_down(a: &[u32], b: &[u32], hunks: &mut [EditHunk]) {
    for idx in (0..hunks.len()).rev() {
        let a_limit = hunks.get(idx + 1).map_or(a.len(), |next| next.a_start);
        let b_limit = hunks.get(idx + 1).map_or(b.len(), |next| next.b_start);
        let h = &mut hunks[idx];
        if h.a_start == h.a_end && h.b_start < h.b_end {
            // Pure insertion: advance while next unchanged line matches first inserted line.
            while h.a_end < a_limit && h.b_end < b_limit && a[h.a_end] == b[h.b_start] {
                h.a_start += 1;
                h.a_end += 1;
                h.b_start += 1;
                h.b_end += 1;
            }
        } else if h.b_start == h.b_end && h.a_start < h.a_end {
            // Pure deletion: advance while next unchanged line matches first deleted line.
            while h.a_end < a_limit && h.b_end < b_limit && a[h.a_start] == a[h.a_end] {
                h.a_start += 1;
                h.a_end += 1;
                h.b_start += 1;
                h.b_end += 1;
            }
        }
    }
}

fn coalesce_touching(raw: Vec<EditHunk>) -> Vec<EditHunk> {
    let mut out: Vec<EditHunk> = Vec::with_capacity(raw.len());
    for h in raw {
        if let Some(last) = out.last_mut()
            && last.a_end >= h.a_start
            && last.b_end >= h.b_start
        {
            last.a_end = last.a_end.max(h.a_end);
            last.b_end = last.b_end.max(h.b_end);
        } else {
            out.push(h);
        }
    }
    out
}

/// Options configuring `merge_trees` / `merge_snapshots`.
#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    /// Explicit conflict markers (`ours`, `base`, `theirs`).
    pub labels: ConflictLabels,
    /// Optional attribute override lines applied after `.gitattributes` inside `main_tree`.
    pub extra_attributes: Vec<String>,
}

/// Classification of one unresolved 3-way Merkle merge conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// Overlapping content edits rendered with `diff3` conflict markers.
    Content,
    /// Divergent additions at a new path (`add/add`), rendered with `diff3` when textual.
    AddAdd,
    /// One side modified a path while the other deleted it (not resolved by exact rename).
    ModifyDelete,
    /// Conflicting binary files (`binary`, `-merge`, `> 100 MiB`, or NUL bytes).
    Binary,
    /// Incompatible mode edits on a new path (`0644` vs `0755`).
    ModeConflict,
    /// Divergent edits to a symlink (`120000`) or symlink vs regular file.
    Symlink,
    /// Divergent edits to a submodule gitlink (`160000`) or gitlink vs file.
    Gitlink,
    /// Directory vs file (`dir <-> leaf`) structural collision.
    DirectoryFile,
    /// Conflicting renames (`1to2` split or `2to1` target collision).
    RenameRename,
    /// Exact rename destination collided with an independent add on the other branch.
    RenameAdd,
    /// Custom `.gitattributes` merge driver is unsupported in pure-Rust CAS merge.
    UnsupportedMergeDriver(String),
}

/// Conflict record surfaced by [`merge_trees`] and [`merge_snapshots`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflict {
    /// Path in the merged tree where the conflict occurred.
    pub path: TreePath,
    /// Conflict classification.
    pub kind: ConflictKind,
    /// Diagnostic explanation.
    pub detail: String,
}

/// Summary returned by [`merge_trees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReport {
    /// Canonical `spawn-tree-v1` root digest (empty tree when empty).
    pub tree: Digest,
    /// Deterministic 40-hex projected Git tree OID for `tree`.
    pub tree_git_oid: GitOid,
    /// All unresolved conflicts (empty when merge succeeded cleanly).
    pub conflicts: Vec<MergeConflict>,
    /// Number of subtrees skipped in `O(1)` (`main == branch`, `base == main`, or `base == branch`).
    pub subtrees_reused: usize,
    /// Number of exact-content renames resolved cleanly during Pass 2.
    pub exact_renames_applied: usize,
}

impl MergeReport {
    /// Whether the three-way merge completed with zero conflicts.
    pub const fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Shadow-mode oracle classification matching `#113` Comment #5 acceptance gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeShadowOutcome {
    /// Both Rust and Git merged cleanly and produced the exact same 40-hex tree OID.
    AgreeClean {
        /// Shared canonical 40-hex tree OID.
        tree_oid: GitOid,
    },
    /// Both Rust and Git reported merge conflicts.
    AgreeConflict {
        /// Conflict count reported by Rust.
        rust_conflicts: usize,
        /// Conflicted path count reported by Git.
        git_conflict_paths: usize,
    },
    /// Blocking divergence: Rust merged cleanly while Git conflicted or diverged on tree OID,
    /// or Rust conflicted on a clean Git merge without a non-exact rename.
    BlockerRustCleanGitConflictOrTreeDiff {
        /// Specific blocking divergence category.
        reason: ShadowBlockerReason,
        /// Rust projected 40-hex tree OID.
        rust_tree_oid: GitOid,
        /// Git native 40-hex tree OID (when produced).
        git_tree_oid: Option<GitOid>,
    },
    /// Explicitly tolerated observation: Git merged cleanly via fuzzy non-exact rename heuristic,
    /// whereas pure-Rust conservative exact-content rename detection flagged a conflict.
    ObservedRustConflictGitCleanNonExactRename {
        /// Conflict count reported by Rust.
        rust_conflicts: usize,
        /// Clean Git 40-hex tree OID.
        git_tree_oid: GitOid,
    },
}

impl MergeShadowOutcome {
    /// Whether this outcome blocks cutover under `#113` Comment #5.
    pub const fn is_blocker(&self) -> bool {
        matches!(self, Self::BlockerRustCleanGitConflictOrTreeDiff { .. })
    }
}

/// Specific reason for [`MergeShadowOutcome::BlockerRustCleanGitConflictOrTreeDiff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowBlockerReason {
    /// Rust falsely succeeded cleanly while Git reported conflicts.
    RustCleanGitConflict,
    /// Both reported clean, but Rust's projected tree OID differed from Git's tree OID.
    CleanTreeOidMismatch,
    /// Git succeeded cleanly without non-exact renames while Rust flagged conflicts.
    UnexpectedRustConflictGitClean,
}

/// Aggregate telemetry counters across synthetic or historical shadow merge runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowOracleSummary {
    /// Total merge cases evaluated (single merge-base).
    pub total: usize,
    /// Clean merges matching Git OID bit-for-bit.
    pub agree_clean: usize,
    /// Conflicted merges where both engines flagged conflict.
    pub agree_conflict: usize,
    /// Hard-blocker divergences (`0` required for cutover).
    pub blockers: usize,
    /// Observed non-exact-rename clean Git merges conservatively conflicted in Rust.
    pub observed_non_exact_rename: usize,
    /// Multi-merge-base commits rejected as expected by the single merge-base guard.
    pub rejected_multi_merge_base: usize,
}

impl ShadowOracleSummary {
    /// Record one single-merge-base shadow comparison result.
    pub fn record(&mut self, outcome: &MergeShadowOutcome) {
        self.total += 1;
        match outcome {
            MergeShadowOutcome::AgreeClean { .. } => self.agree_clean += 1,
            MergeShadowOutcome::AgreeConflict { .. } => self.agree_conflict += 1,
            MergeShadowOutcome::BlockerRustCleanGitConflictOrTreeDiff { .. } => {
                self.blockers += 1;
            }
            MergeShadowOutcome::ObservedRustConflictGitCleanNonExactRename { .. } => {
                self.observed_non_exact_rename += 1;
            }
        }
    }
}

/// Classify a pair of Rust (`merge_trees`) and Git (`merge-tree`) results under
/// `#113` Comment #5's shadow admission rule.
pub fn classify_shadow_merge(
    rust_report: &MergeReport,
    git_clean: bool,
    git_tree_oid: Option<GitOid>,
    git_conflict_paths: usize,
    git_had_non_exact_rename: bool,
) -> MergeShadowOutcome {
    if rust_report.is_clean() {
        if !git_clean {
            return MergeShadowOutcome::BlockerRustCleanGitConflictOrTreeDiff {
                reason: ShadowBlockerReason::RustCleanGitConflict,
                rust_tree_oid: rust_report.tree_git_oid,
                git_tree_oid,
            };
        }
        return match git_tree_oid {
            Some(oid) if oid == rust_report.tree_git_oid => {
                MergeShadowOutcome::AgreeClean { tree_oid: oid }
            }
            other => MergeShadowOutcome::BlockerRustCleanGitConflictOrTreeDiff {
                reason: ShadowBlockerReason::CleanTreeOidMismatch,
                rust_tree_oid: rust_report.tree_git_oid,
                git_tree_oid: other,
            },
        };
    }

    if !git_clean {
        return MergeShadowOutcome::AgreeConflict {
            rust_conflicts: rust_report.conflicts.len(),
            git_conflict_paths,
        };
    }

    if git_had_non_exact_rename && let Some(oid) = git_tree_oid {
        return MergeShadowOutcome::ObservedRustConflictGitCleanNonExactRename {
            rust_conflicts: rust_report.conflicts.len(),
            git_tree_oid: oid,
        };
    }

    MergeShadowOutcome::BlockerRustCleanGitConflictOrTreeDiff {
        reason: ShadowBlockerReason::UnexpectedRustConflictGitClean,
        rust_tree_oid: rust_report.tree_git_oid,
        git_tree_oid,
    }
}

/// Three-way merge of canonical `spawn-tree-v1` trees (`base_tree` -> `main_tree`, `branch_tree`),
/// returning the merged root tree digest, its 40-hex projected OID, conflicts, and statistics.
pub fn merge_trees(
    store: &CasMerkleStore,
    base_tree: &Digest,
    main_tree: &Digest,
    branch_tree: &Digest,
    options: &MergeOptions,
) -> Result<MergeReport, D6StoreError> {
    if main_tree == branch_tree {
        return fast_path_report(store, main_tree);
    }
    if base_tree == main_tree {
        return fast_path_report(store, branch_tree);
    }
    if base_tree == branch_tree {
        return fast_path_report(store, main_tree);
    }

    let mut attrs = GitAttributes::default();
    attrs.set_override_lines(&options.extra_attributes)?;

    let mut ctx = TreeMergeCtx {
        store,
        attrs: &mut attrs,
        options,
        conflicts: Vec::new(),
        subtrees_reused: 0,
        rename_triggers: Vec::new(),
    };

    let merged_opt =
        ctx.merge_dir_nodes(b"", Some(base_tree), Some(main_tree), Some(branch_tree))?;
    let mut tree = match merged_opt {
        Some(d) => d,
        None => store.cas().put_empty_tree()?,
    };
    let mut subtrees_reused = ctx.subtrees_reused;
    let mut conflicts = ctx.conflicts;
    let mut exact_renames_applied = 0_usize;

    if !ctx.rename_triggers.is_empty() {
        let rename_res = resolve_exact_renames(
            store,
            base_tree,
            main_tree,
            branch_tree,
            &tree,
            options,
            &ctx.rename_triggers,
            &mut conflicts,
        )?;
        tree = rename_res.tree;
        exact_renames_applied = rename_res.applied;
        if rename_res.applied > 0 {
            // Re-run subtree count stays additive from pass 1.
            subtrees_reused += rename_res.extra_subtrees_reused;
        }
    }

    conflicts.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let tree_git_oid = store.project_tree_oid(&tree)?;

    Ok(MergeReport {
        tree,
        tree_git_oid,
        conflicts,
        subtrees_reused,
        exact_renames_applied,
    })
}

fn fast_path_report(store: &CasMerkleStore, root: &Digest) -> Result<MergeReport, D6StoreError> {
    let ensured = if *root == empty_tree_digest() && !store.cas().has(root) {
        store.cas().put_empty_tree()?
    } else {
        root.clone()
    };
    let tree_git_oid = store.project_tree_oid(&ensured)?;
    Ok(MergeReport {
        tree: ensured,
        tree_git_oid,
        conflicts: Vec::new(),
        subtrees_reused: 1,
        exact_renames_applied: 0,
    })
}

/// End-to-end 3-way snapshot merge obeying the single merge-base guard: finds
/// the unique common ancestor of `main_snapshot` and `branch_snapshot` (refusing
/// `0` or `> 1` merge-bases) and delegates tree merging to [`merge_trees`].
pub fn merge_snapshots(
    store: &CasMerkleStore,
    main_snapshot: &Digest,
    branch_snapshot: &Digest,
    options: Option<MergeOptions>,
) -> Result<(Digest, MergeReport), MergeBaseError> {
    let base_snapshot = find_single_merge_base(store, main_snapshot, branch_snapshot)?;
    let base_snap = store.cas().read_merkle_snapshot(&base_snapshot)?;
    let main_snap = store.cas().read_merkle_snapshot(main_snapshot)?;
    let branch_snap = store.cas().read_merkle_snapshot(branch_snapshot)?;

    let resolved_opts = if let Some(opts) = options {
        opts
    } else {
        let mut cache = HashMap::new();
        let base_oid = snapshot_oid_cached(store, &base_snapshot, &mut cache)?;
        let main_oid = snapshot_oid_cached(store, main_snapshot, &mut cache)?;
        let branch_oid = snapshot_oid_cached(store, branch_snapshot, &mut cache)?;
        MergeOptions {
            labels: ConflictLabels {
                ours: main_oid.to_hex(),
                base: base_oid.to_hex(),
                theirs: branch_oid.to_hex(),
            },
            extra_attributes: Vec::new(),
        }
    };

    let report = merge_trees(
        store,
        &base_snap.tree,
        &main_snap.tree,
        &branch_snap.tree,
        &resolved_opts,
    )?;
    Ok((base_snapshot, report))
}

fn snapshot_oid_cached(
    store: &CasMerkleStore,
    id: &Digest,
    cache: &mut HashMap<Digest, GitOid>,
) -> Result<GitOid, D6StoreError> {
    if let Some(&oid) = cache.get(id) {
        return Ok(oid);
    }
    let snap = store.cas().read_merkle_snapshot(id)?;
    let mut parent_oids = Vec::with_capacity(snap.parents.len());
    for p in &snap.parents {
        parent_oids.push(snapshot_oid_cached(store, p, cache)?);
    }
    let oid = store.project_snapshot_oid(&snap, &parent_oids)?;
    cache.insert(id.clone(), oid);
    Ok(oid)
}

#[derive(Debug, Clone)]
enum RenameTrigger {
    /// Entry was modified on `main_is_survivor` and deleted on the other side, OR deleted on one side.
    ModifyDelete {
        path: Vec<u8>,
        surviving_on_main: bool,
    },
    /// Entry existed in base and disappeared on both sides (possible 1->2 rename/rename),
    /// or was added/collided at the target path.
    PathCollision,
}

struct TreeMergeCtx<'a> {
    store: &'a CasMerkleStore,
    attrs: &'a mut GitAttributes,
    options: &'a MergeOptions,
    conflicts: Vec<MergeConflict>,
    subtrees_reused: usize,
    rename_triggers: Vec<RenameTrigger>,
}

impl TreeMergeCtx<'_> {
    fn read_opt_tree(&self, digest: Option<&Digest>) -> Result<Tree, D6StoreError> {
        match digest {
            Some(d) => self.store.read_tree_cached(d),
            None => Ok(Tree::default()),
        }
    }

    #[expect(clippy::too_many_lines)]
    fn merge_dir_nodes(
        &mut self,
        rel_dir: &[u8],
        b_opt: Option<&Digest>,
        m_opt: Option<&Digest>,
        r_opt: Option<&Digest>,
    ) -> Result<Option<Digest>, D6StoreError> {
        // O(1) structural subtree short-circuits:
        if m_opt == r_opt {
            self.subtrees_reused += 1;
            return Ok(m_opt.cloned());
        }
        if b_opt == m_opt {
            self.subtrees_reused += 1;
            return Ok(r_opt.cloned());
        }
        if b_opt == r_opt {
            self.subtrees_reused += 1;
            return Ok(m_opt.cloned());
        }

        let b_tree = self.read_opt_tree(b_opt)?;
        let m_tree = self.read_opt_tree(m_opt)?;
        let r_tree = self.read_opt_tree(r_opt)?;

        // Lazily push `.gitattributes` from `main` subtree if present at this level.
        let pushed_attr = if let Ok(idx) = m_tree
            .entries
            .binary_search_by(|e| e.name.as_slice().cmp(b".gitattributes"))
        {
            let attr_entry = &m_tree.entries[idx];
            if matches!(
                attr_entry.kind,
                TreeEntryKind::File | TreeEntryKind::Executable
            ) && let Some(attr_digest) = attr_entry.digest()
            {
                let blob = self.store.cas().read_blob(&attr_digest)?;
                self.attrs.push_dir_bytes(rel_dir, &blob)?;
                true
            } else {
                false
            }
        } else {
            false
        };

        let mut merged_entries: Vec<TreeEntry> = Vec::new();
        let mut bi = 0_usize;
        let mut mi = 0_usize;
        let mut ri = 0_usize;

        while bi < b_tree.entries.len() || mi < m_tree.entries.len() || ri < r_tree.entries.len() {
            let min_name = min_entry_name(
                b_tree.entries.get(bi),
                m_tree.entries.get(mi),
                r_tree.entries.get(ri),
            );
            let b_ent = take_matching(&b_tree.entries, &mut bi, &min_name);
            let m_ent = take_matching(&m_tree.entries, &mut mi, &min_name);
            let r_ent = take_matching(&r_tree.entries, &mut ri, &min_name);

            let child_rel = join_rel_path(rel_dir, &min_name);
            let any_dir = [b_ent, m_ent, r_ent]
                .into_iter()
                .flatten()
                .any(|e| e.kind == TreeEntryKind::Dir);

            if any_dir {
                let b_is_dir = b_ent.is_some_and(|e| e.kind == TreeEntryKind::Dir);
                let m_is_dir = m_ent.is_some_and(|e| e.kind == TreeEntryKind::Dir);
                let r_is_dir = r_ent.is_some_and(|e| e.kind == TreeEntryKind::Dir);
                let all_present_dir = [b_ent, m_ent, r_ent]
                    .into_iter()
                    .flatten()
                    .all(|e| e.kind == TreeEntryKind::Dir);

                if all_present_dir {
                    let b_sub = b_ent.and_then(TreeEntry::digest);
                    let m_sub = m_ent.and_then(TreeEntry::digest);
                    let r_sub = r_ent.and_then(TreeEntry::digest);
                    if let Some(sub_digest) = self.merge_dir_nodes(
                        &child_rel,
                        b_sub.as_ref(),
                        m_sub.as_ref(),
                        r_sub.as_ref(),
                    )? {
                        merged_entries.push(TreeEntry::dir(min_name, &sub_digest));
                    }
                    continue;
                }

                // Mixed Dir / non-Dir handling: clean if one side kept base unchanged.
                if m_ent == b_ent && !r_is_dir {
                    if let Some(re) = r_ent {
                        merged_entries.push(re.clone());
                    }
                    continue;
                }
                if r_ent == b_ent && !m_is_dir {
                    if let Some(me) = m_ent {
                        merged_entries.push(me.clone());
                    }
                    continue;
                }
                if m_ent == r_ent && !b_is_dir {
                    if let Some(me) = m_ent {
                        merged_entries.push(me.clone());
                    }
                    continue;
                }
                if (m_is_dir && r_is_dir && b_ent.is_some())
                    || (b_is_dir && m_ent.is_none() && !r_is_dir)
                {
                    // Handled conservatively as dir/file collision.
                }

                self.rename_triggers.push(RenameTrigger::PathCollision);
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(child_rel),
                    kind: ConflictKind::DirectoryFile,
                    detail: "directory vs file collision across branches".to_string(),
                });
                if let Some(preferred) = m_ent.or(r_ent) {
                    merged_entries.push(preferred.clone());
                }
                continue;
            }

            // All present entries at `min_name` are non-Dir leaves.
            if let Some(merged_leaf) =
                self.merge_leaf(&child_rel, &min_name, b_ent, m_ent, r_ent)?
            {
                merged_entries.push(merged_leaf);
            }
        }

        if pushed_attr {
            self.attrs.pop_layer();
        }

        if merged_entries.is_empty() {
            return Ok(None);
        }
        let tree = Tree::from_entries(merged_entries)?;
        let digest = self.store.put_tree_cached(&tree)?;
        Ok(Some(digest))
    }

    fn merge_leaf(
        &mut self,
        rel_path: &[u8],
        name: &[u8],
        b_ent: Option<&TreeEntry>,
        m_ent: Option<&TreeEntry>,
        r_ent: Option<&TreeEntry>,
    ) -> Result<Option<TreeEntry>, D6StoreError> {
        if m_ent == r_ent {
            if b_ent.is_some() && m_ent.is_none() {
                self.rename_triggers.push(RenameTrigger::PathCollision);
            }
            return Ok(m_ent.cloned());
        }
        if b_ent == m_ent {
            return Ok(r_ent.cloned());
        }
        if b_ent == r_ent {
            return Ok(m_ent.cloned());
        }

        match (b_ent, m_ent, r_ent) {
            (Some(_b), Some(m), None) => {
                // Modified in main, deleted in branch -> candidate for branch rename or modify/delete.
                self.rename_triggers.push(RenameTrigger::ModifyDelete {
                    path: rel_path.to_vec(),
                    surviving_on_main: true,
                });
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(rel_path),
                    kind: ConflictKind::ModifyDelete,
                    detail: "modified on mainline and deleted on branch".to_string(),
                });
                Ok(Some(m.clone()))
            }
            (Some(_b), None, Some(r)) => {
                // Deleted in main, modified in branch -> candidate for main rename or modify/delete.
                self.rename_triggers.push(RenameTrigger::ModifyDelete {
                    path: rel_path.to_vec(),
                    surviving_on_main: false,
                });
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(rel_path),
                    kind: ConflictKind::ModifyDelete,
                    detail: "deleted on mainline and modified on branch".to_string(),
                });
                Ok(Some(r.clone()))
            }
            (None, Some(m), Some(r)) => {
                // Add/Add: record trigger in case one or both sides came from an exact rename.
                self.rename_triggers.push(RenameTrigger::PathCollision);
                self.merge_two_leaves(rel_path, name, None, m, r).map(Some)
            }
            (Some(b), Some(m), Some(r)) => self
                .merge_two_leaves(rel_path, name, Some(b), m, r)
                .map(Some),
            (None, Some(_) | None, None) | (None, None, Some(_)) | (Some(_), None, None) => {
                unreachable!("handled by equality short-circuits")
            }
        }
    }

    #[expect(clippy::too_many_lines)]
    fn merge_two_leaves(
        &mut self,
        rel_path: &[u8],
        name: &[u8],
        b_opt: Option<&TreeEntry>,
        m: &TreeEntry,
        r: &TreeEntry,
    ) -> Result<TreeEntry, D6StoreError> {
        let is_add_add = b_opt.is_none();
        // Resolve entry kinds (`File` and `Executable` can merge modes; `Symlink`/`Gitlink` cannot).
        let (resolved_kind, mode_conflict) =
            merge_leaf_kinds(b_opt.map(|e| e.kind), m.kind, r.kind);
        let Some(merged_kind) = resolved_kind else {
            let kind = match (m.kind, r.kind) {
                (TreeEntryKind::Gitlink, _) | (_, TreeEntryKind::Gitlink) => ConflictKind::Gitlink,
                (TreeEntryKind::Symlink, _) | (_, TreeEntryKind::Symlink) => ConflictKind::Symlink,
                _ => ConflictKind::ModeConflict,
            };
            self.conflicts.push(MergeConflict {
                path: TreePath::from_bytes(rel_path),
                kind,
                detail: format!("incompatible entry kinds: {:?} vs {:?}", m.kind, r.kind),
            });
            return Ok(m.clone());
        };

        match merged_kind {
            TreeEntryKind::Symlink => {
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(rel_path),
                    kind: ConflictKind::Symlink,
                    detail: "conflicting symlink target modifications".to_string(),
                });
                return Ok(m.clone());
            }
            TreeEntryKind::Gitlink => {
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(rel_path),
                    kind: ConflictKind::Gitlink,
                    detail: "conflicting submodule gitlink commits".to_string(),
                });
                return Ok(m.clone());
            }
            TreeEntryKind::Dir => {
                unreachable!("Dir handled before merge_two_leaves")
            }
            TreeEntryKind::File | TreeEntryKind::Executable => {}
        }

        if mode_conflict {
            self.conflicts.push(MergeConflict {
                path: TreePath::from_bytes(rel_path),
                kind: ConflictKind::ModeConflict,
                detail: "mode conflict on newly added regular file (0644 vs 0755)".to_string(),
            });
            return Ok(m.clone());
        }

        // If contents are identical (only executable bit changed cleanly on one side):
        if m.id == r.id && m.size == r.size {
            let digest = m.digest().expect("file has digest");
            return Ok(make_regular_entry(name, merged_kind, &digest, m.size));
        }

        // Check pointer threshold before reading blobs into memory.
        let any_oversized = b_opt.is_some_and(|b| b.size > GIT_POINTER_THRESHOLD)
            || m.size > GIT_POINTER_THRESHOLD
            || r.size > GIT_POINTER_THRESHOLD;
        if any_oversized {
            self.conflicts.push(MergeConflict {
                path: TreePath::from_bytes(rel_path),
                kind: ConflictKind::Binary,
                detail: "blob exceeds GIT_POINTER_THRESHOLD (100 MiB) binary cap".to_string(),
            });
            return Ok(m.clone());
        }

        let path_attrs = self.attrs.resolve_path(rel_path);
        match &path_attrs.merge {
            MergeDriver::Binary => {
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(rel_path),
                    kind: ConflictKind::Binary,
                    detail: "binary attribute or -merge forbids 3-way line merge".to_string(),
                });
                return Ok(m.clone());
            }
            MergeDriver::Custom(driver_name) => {
                self.conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(rel_path),
                    kind: ConflictKind::UnsupportedMergeDriver(driver_name.clone()),
                    detail: format!("unsupported custom merge driver merge={driver_name}"),
                });
                return Ok(m.clone());
            }
            MergeDriver::Text | MergeDriver::Union => {}
        }

        let b_bytes = match b_opt.and_then(TreeEntry::digest) {
            Some(d) => self.store.cas().read_blob(&d)?,
            None => Vec::new(),
        };
        let m_bytes = self
            .store
            .cas()
            .read_blob(&m.digest().expect("file digest"))?;
        let r_bytes = self
            .store
            .cas()
            .read_blob(&r.digest().expect("file digest"))?;

        if is_binary_buffer(&b_bytes) || is_binary_buffer(&m_bytes) || is_binary_buffer(&r_bytes) {
            self.conflicts.push(MergeConflict {
                path: TreePath::from_bytes(rel_path),
                kind: ConflictKind::Binary,
                detail: "NUL byte detected in file contents (binary merge conflict)".to_string(),
            });
            return Ok(m.clone());
        }

        let merged = merge_text_3way(
            &b_bytes,
            &m_bytes,
            &r_bytes,
            &path_attrs.merge,
            path_attrs.conflict_marker_size,
            &self.options.labels,
        );

        let (blob_digest, blob_size) = self.store.put_bytes_blob(&merged.bytes)?;
        if merged.conflicted {
            self.conflicts.push(MergeConflict {
                path: TreePath::from_bytes(rel_path),
                kind: if is_add_add {
                    ConflictKind::AddAdd
                } else {
                    ConflictKind::Content
                },
                detail: if is_add_add {
                    "add/add content conflict with diff3 markers".to_string()
                } else {
                    "3-way line content conflict with diff3 markers".to_string()
                },
            });
        }
        Ok(make_regular_entry(
            name,
            merged_kind,
            &blob_digest,
            blob_size,
        ))
    }
}

fn make_regular_entry(name: &[u8], kind: TreeEntryKind, digest: &Digest, size: u64) -> TreeEntry {
    match kind {
        TreeEntryKind::Executable => TreeEntry::executable(name, digest, size),
        _ => TreeEntry::file(name, digest, size),
    }
}

fn merge_leaf_kinds(
    b_kind: Option<TreeEntryKind>,
    m_kind: TreeEntryKind,
    r_kind: TreeEntryKind,
) -> (Option<TreeEntryKind>, bool) {
    if m_kind == r_kind {
        return (Some(m_kind), false);
    }
    let is_reg = |k: TreeEntryKind| matches!(k, TreeEntryKind::File | TreeEntryKind::Executable);
    if !is_reg(m_kind) || !is_reg(r_kind) {
        return (None, false);
    }
    match b_kind {
        Some(bk) if bk == m_kind => (Some(r_kind), false),
        Some(bk) if bk == r_kind => (Some(m_kind), false),
        Some(_) => (Some(m_kind), false),
        None => (Some(m_kind), true),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContentIdentity {
    kind: TreeEntryKind,
    id: [u8; 32],
    size: u64,
}

impl ContentIdentity {
    fn from_entry(e: &TreeEntry) -> Self {
        let normalized_kind = match e.kind {
            TreeEntryKind::File | TreeEntryKind::Executable => TreeEntryKind::File,
            other => other,
        };
        Self {
            kind: normalized_kind,
            id: e.id,
            size: e.size,
        }
    }
}

#[derive(Default)]
struct SideDiff {
    deleted: BTreeMap<Vec<u8>, TreeEntry>,
    added: BTreeMap<Vec<u8>, TreeEntry>,
}

struct RenameResolution {
    tree: Digest,
    applied: usize,
    extra_subtrees_reused: usize,
}

#[expect(clippy::too_many_arguments, clippy::too_many_lines)]
fn resolve_exact_renames(
    store: &CasMerkleStore,
    base_tree: &Digest,
    main_tree: &Digest,
    branch_tree: &Digest,
    pass1_tree: &Digest,
    options: &MergeOptions,
    triggers: &[RenameTrigger],
    conflicts: &mut Vec<MergeConflict>,
) -> Result<RenameResolution, D6StoreError> {
    let mut main_diff = SideDiff::default();
    let mut branch_diff = SideDiff::default();
    let mut extra_reused = 0_usize;

    collect_side_diffs(
        store,
        b"",
        Some(base_tree),
        Some(main_tree),
        Some(branch_tree),
        &mut main_diff,
        &mut branch_diff,
        &mut extra_reused,
    )?;

    let main_renames = detect_unique_exact_renames(&main_diff);
    let branch_renames = detect_unique_exact_renames(&branch_diff);

    if main_renames.is_empty() && branch_renames.is_empty() {
        return Ok(RenameResolution {
            tree: pass1_tree.clone(),
            applied: 0,
            extra_subtrees_reused: 0,
        });
    }

    let mut attrs = GitAttributes::default();
    attrs.set_override_lines(&options.extra_attributes)?;
    load_all_attrs_in_tree(store, b"", main_tree, &mut attrs)?;

    let mut flat_result = store.flatten_tree(pass1_tree)?;
    let main_flat = store.flatten_tree(main_tree)?;
    let branch_flat = store.flatten_tree(branch_tree)?;

    let mut applied = 0_usize;
    let mut tree_dirty = false;
    let mut clear_paths: BTreeSet<Vec<u8>> = BTreeSet::new();

    // Reverse lookup maps dst -> src
    let main_dst_to_src: HashMap<Vec<u8>, Vec<u8>> = main_renames
        .iter()
        .map(|(s, d)| (d.clone(), s.clone()))
        .collect();
    let branch_dst_to_src: HashMap<Vec<u8>, Vec<u8>> = branch_renames
        .iter()
        .map(|(s, d)| (d.clone(), s.clone()))
        .collect();

    for trigger in triggers {
        let RenameTrigger::ModifyDelete {
            path: src,
            surviving_on_main,
        } = trigger
        else {
            continue;
        };

        let (renaming_map, other_dst_to_src, surviving_side_flat, deleted_side_added) =
            if *surviving_on_main {
                (
                    &branch_renames,
                    &main_dst_to_src,
                    &main_flat,
                    &branch_diff.added,
                )
            } else {
                (
                    &main_renames,
                    &branch_dst_to_src,
                    &branch_flat,
                    &main_diff.added,
                )
            };

        let Some(dst) = renaming_map.get(src) else {
            continue;
        };

        // Case A: Both sides exact-renamed `src`:
        // Check if surviving_side ALSO renamed `src -> some_other_dst`: unreachable if `surviving_on_main`
        // (since surviving_side kept `src` as modified). What if `main_renames` ALSO exists?
        let surviving_entry = surviving_side_flat.get(src);
        let renamed_entry = if *surviving_on_main {
            branch_flat.get(dst)
        } else {
            main_flat.get(dst)
        };
        let (Some(mod_ent), Some(ren_ent)) = (surviving_entry, renamed_entry) else {
            continue;
        };

        // Check for rename/add collision if surviving side independently added `dst`.
        if let Some(other_add) = surviving_side_flat.get(dst) {
            // Did surviving side modify/add `dst` independently without renaming `src`?
            let base_had_dst = other_dst_to_src.contains_key(dst);
            if !base_had_dst && other_add != ren_ent {
                conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(dst.clone()),
                    kind: ConflictKind::RenameAdd,
                    detail: format!(
                        "exact rename {} -> {} collides with independent file at destination",
                        String::from_utf8_lossy(src),
                        String::from_utf8_lossy(dst),
                    ),
                });
                continue;
            }
        }
        let _ = deleted_side_added;

        // Check directory/file collision at `dst`.
        if has_dir_conflict_at(&flat_result, dst) {
            conflicts.push(MergeConflict {
                path: TreePath::from_bytes(dst.clone()),
                kind: ConflictKind::DirectoryFile,
                detail: "exact rename target collides with existing directory/file".to_string(),
            });
            continue;
        }

        // Resolve: move surviving side's content/mode edits to `dst`, remove `src`.
        let merged_at_dst =
            merge_rename_destination_entry(dst, ren_ent, mod_ent, *surviving_on_main, conflicts);
        flat_result.remove(src);
        flat_result.insert(dst.clone(), merged_at_dst);
        clear_paths.insert(src.clone());
        applied += 1;
        tree_dirty = true;
    }

    // Check rename/rename (1to2) and (2to1) cases among remaining triggers:
    for (src, dst_main) in &main_renames {
        if let Some(dst_branch) = branch_renames.get(src) {
            if dst_main == dst_branch {
                if flat_result.remove(src).is_some() {
                    tree_dirty = true;
                }
                clear_paths.insert(src.clone());
                applied += 1;
            } else {
                conflicts.push(MergeConflict {
                    path: TreePath::from_bytes(src.clone()),
                    kind: ConflictKind::RenameRename,
                    detail: format!(
                        "rename/rename (1to2): {} -> {} vs {}",
                        String::from_utf8_lossy(src),
                        String::from_utf8_lossy(dst_main),
                        String::from_utf8_lossy(dst_branch)
                    ),
                });
            }
        }
    }
    for (dst, src_main) in &main_dst_to_src {
        if let Some(src_branch) = branch_dst_to_src.get(dst)
            && src_main != src_branch
        {
            conflicts.push(MergeConflict {
                path: TreePath::from_bytes(dst.clone()),
                kind: ConflictKind::RenameRename,
                detail: format!(
                    "rename/rename (2to1) onto {}: {} vs {}",
                    String::from_utf8_lossy(dst),
                    String::from_utf8_lossy(src_main),
                    String::from_utf8_lossy(src_branch)
                ),
            });
        }
    }

    // Also upgrade Add/Add conflict at `dst` into RenameAdd when exactly one side renamed into `dst`.
    for c in conflicts.iter_mut() {
        if c.kind == ConflictKind::AddAdd {
            let p = c.path.as_bytes();
            if main_dst_to_src.contains_key(p) ^ branch_dst_to_src.contains_key(p) {
                c.kind = ConflictKind::RenameAdd;
            }
        }
    }

    if !clear_paths.is_empty() {
        conflicts.retain(|c| {
            !(c.kind == ConflictKind::ModifyDelete && clear_paths.contains(c.path.as_bytes()))
        });
    }

    if !tree_dirty {
        return Ok(RenameResolution {
            tree: pass1_tree.clone(),
            applied,
            extra_subtrees_reused: 0,
        });
    }

    let rebuilt = build_tree_from_flat(store, &flat_result)?;
    Ok(RenameResolution {
        tree: rebuilt,
        applied,
        extra_subtrees_reused: extra_reused,
    })
}

fn merge_rename_destination_entry(
    dst: &[u8],
    base_and_renamer: &TreeEntry,
    modifier: &TreeEntry,
    modifier_is_main: bool,
    conflicts: &mut Vec<MergeConflict>,
) -> TreeEntry {
    let dst_name = dst.rsplit(|&b| b == b'/').next().unwrap_or(dst);
    let (m_kind, r_kind) = if modifier_is_main {
        (modifier.kind, base_and_renamer.kind)
    } else {
        (base_and_renamer.kind, modifier.kind)
    };
    let (resolved_kind, _) = merge_leaf_kinds(Some(base_and_renamer.kind), m_kind, r_kind);
    let Some(kind) = resolved_kind else {
        conflicts.push(MergeConflict {
            path: TreePath::from_bytes(dst),
            kind: ConflictKind::ModeConflict,
            detail: "incompatible entry kind during exact rename merge".to_string(),
        });
        return TreeEntry {
            kind: modifier.kind,
            name: dst_name.to_vec(),
            id: modifier.id,
            size: modifier.size,
        };
    };
    TreeEntry {
        kind,
        name: dst_name.to_vec(),
        id: modifier.id,
        size: modifier.size,
    }
}

fn has_dir_conflict_at(flat: &BTreeMap<Vec<u8>, TreeEntry>, path: &[u8]) -> bool {
    if flat.contains_key(path) {
        return false;
    }
    let mut prefix = Vec::with_capacity(path.len() + 1);
    prefix.extend_from_slice(path);
    prefix.push(b'/');
    if flat
        .range(prefix.clone()..)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&prefix))
    {
        return true;
    }
    let mut off = 0;
    while let Some(pos) = path[off..].iter().position(|&b| b == b'/') {
        let cut = off + pos;
        if flat.contains_key(&path[..cut]) {
            return true;
        }
        off = cut + 1;
    }
    false
}

fn load_all_attrs_in_tree(
    store: &CasMerkleStore,
    rel_dir: &[u8],
    digest: &Digest,
    attrs: &mut GitAttributes,
) -> Result<(), D6StoreError> {
    let tree = store.read_tree_cached(digest)?;
    if let Ok(idx) = tree
        .entries
        .binary_search_by(|e| e.name.as_slice().cmp(b".gitattributes"))
    {
        let e = &tree.entries[idx];
        if matches!(e.kind, TreeEntryKind::File | TreeEntryKind::Executable)
            && let Some(d) = e.digest()
        {
            let bytes = store.cas().read_blob(&d)?;
            attrs.push_dir_bytes(rel_dir, &bytes)?;
        }
    }
    for e in tree.entries {
        if e.kind == TreeEntryKind::Dir
            && let Some(sub) = e.digest()
        {
            let sub_rel = join_rel_path(rel_dir, &e.name);
            load_all_attrs_in_tree(store, &sub_rel, &sub, attrs)?;
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn collect_side_diffs(
    store: &CasMerkleStore,
    rel_dir: &[u8],
    b_opt: Option<&Digest>,
    m_opt: Option<&Digest>,
    r_opt: Option<&Digest>,
    main_diff: &mut SideDiff,
    branch_diff: &mut SideDiff,
    reused: &mut usize,
) -> Result<(), D6StoreError> {
    if b_opt == m_opt && b_opt == r_opt {
        *reused += 1;
        return Ok(());
    }
    let read_opt = |opt: Option<&Digest>| -> Result<Tree, D6StoreError> {
        match opt {
            Some(d) => store.read_tree_cached(d),
            None => Ok(Tree::default()),
        }
    };
    let b_tree = read_opt(b_opt)?;
    let m_tree = read_opt(m_opt)?;
    let r_tree = read_opt(r_opt)?;

    let mut bi = 0_usize;
    let mut mi = 0_usize;
    let mut ri = 0_usize;

    while bi < b_tree.entries.len() || mi < m_tree.entries.len() || ri < r_tree.entries.len() {
        let min_name = min_entry_name(
            b_tree.entries.get(bi),
            m_tree.entries.get(mi),
            r_tree.entries.get(ri),
        );
        let b_ent = take_matching(&b_tree.entries, &mut bi, &min_name);
        let m_ent = take_matching(&m_tree.entries, &mut mi, &min_name);
        let r_ent = take_matching(&r_tree.entries, &mut ri, &min_name);

        if b_ent == m_ent && b_ent == r_ent {
            *reused += 1;
            continue;
        }

        let child_rel = join_rel_path(rel_dir, &min_name);
        let any_dir = [b_ent, m_ent, r_ent]
            .into_iter()
            .flatten()
            .any(|e| e.kind == TreeEntryKind::Dir);
        if any_dir {
            // Emit non-dir side entries at this path if mixed dir/leaf, and recurse into dir subtrees.
            record_leaf_diff(b_ent, m_ent, &child_rel, main_diff);
            record_leaf_diff(b_ent, r_ent, &child_rel, branch_diff);
            collect_side_diffs(
                store,
                &child_rel,
                b_ent
                    .filter(|e| e.kind == TreeEntryKind::Dir)
                    .and_then(TreeEntry::digest)
                    .as_ref(),
                m_ent
                    .filter(|e| e.kind == TreeEntryKind::Dir)
                    .and_then(TreeEntry::digest)
                    .as_ref(),
                r_ent
                    .filter(|e| e.kind == TreeEntryKind::Dir)
                    .and_then(TreeEntry::digest)
                    .as_ref(),
                main_diff,
                branch_diff,
                reused,
            )?;
        } else {
            record_leaf_diff(b_ent, m_ent, &child_rel, main_diff);
            record_leaf_diff(b_ent, r_ent, &child_rel, branch_diff);
        }
    }
    Ok(())
}

fn record_leaf_diff(
    b_ent: Option<&TreeEntry>,
    s_ent: Option<&TreeEntry>,
    rel_path: &[u8],
    diff: &mut SideDiff,
) {
    let b_leaf = b_ent.filter(|e| e.kind != TreeEntryKind::Dir);
    let s_leaf = s_ent.filter(|e| e.kind != TreeEntryKind::Dir);
    match (b_leaf, s_leaf) {
        (Some(b), None) => {
            diff.deleted.insert(rel_path.to_vec(), b.clone());
        }
        (None, Some(s)) => {
            diff.added.insert(rel_path.to_vec(), s.clone());
        }
        _ => {}
    }
}

/// Pair exact-content deleted <-> added leaves when each content identity occurs
/// uniquely once on the deleted side and uniquely once on the added side.
fn detect_unique_exact_renames(diff: &SideDiff) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut del_by_id: HashMap<ContentIdentity, Vec<&[u8]>> = HashMap::new();
    let mut add_by_id: HashMap<ContentIdentity, Vec<&[u8]>> = HashMap::new();
    for (p, e) in &diff.deleted {
        del_by_id
            .entry(ContentIdentity::from_entry(e))
            .or_default()
            .push(p.as_slice());
    }
    for (p, e) in &diff.added {
        add_by_id
            .entry(ContentIdentity::from_entry(e))
            .or_default()
            .push(p.as_slice());
    }

    let mut renames = BTreeMap::new();
    for (id, srcs) in del_by_id {
        if srcs.len() != 1 {
            continue;
        }
        if let Some(dsts) = add_by_id.get(&id)
            && dsts.len() == 1
        {
            renames.insert(srcs[0].to_vec(), dsts[0].to_vec());
        }
    }
    renames
}

fn build_tree_from_flat(
    store: &CasMerkleStore,
    flat: &BTreeMap<Vec<u8>, TreeEntry>,
) -> Result<Digest, D6StoreError> {
    if flat.is_empty() {
        return Ok(store.cas().put_empty_tree()?);
    }
    build_subtree_recursive(store, flat.iter().map(|(k, v)| (k.as_slice(), v)).collect())
}

fn build_subtree_recursive(
    store: &CasMerkleStore,
    items: Vec<(&[u8], &TreeEntry)>,
) -> Result<Digest, D6StoreError> {
    let mut groups: BTreeMap<&[u8], Vec<(&[u8], &TreeEntry)>> = BTreeMap::new();
    let mut leaves: Vec<TreeEntry> = Vec::new();
    for (rel, ent) in items {
        if let Some(slash) = rel.iter().position(|&b| b == b'/') {
            let dir_name = &rel[..slash];
            let rest = &rel[slash + 1..];
            groups.entry(dir_name).or_default().push((rest, ent));
        } else {
            leaves.push(TreeEntry {
                kind: ent.kind,
                name: rel.to_vec(),
                id: ent.id,
                size: ent.size,
            });
        }
    }
    for (dir_name, sub_items) in groups {
        let sub_digest = build_subtree_recursive(store, sub_items)?;
        leaves.push(TreeEntry::dir(dir_name, &sub_digest));
    }
    let tree = Tree::from_entries(leaves)?;
    store.put_tree_cached(&tree)
}

fn min_entry_name(a: Option<&TreeEntry>, b: Option<&TreeEntry>, c: Option<&TreeEntry>) -> Vec<u8> {
    let mut best: Option<&[u8]> = None;
    for candidate in [a, b, c].into_iter().flatten() {
        best = Some(match best {
            Some(cur) if cur.cmp(candidate.name.as_slice()) != Ordering::Greater => cur,
            _ => candidate.name.as_slice(),
        });
    }
    best.expect("at least one entry exists").to_vec()
}

fn take_matching<'a>(
    entries: &'a [TreeEntry],
    idx: &mut usize,
    target: &[u8],
) -> Option<&'a TreeEntry> {
    let ent = entries.get(*idx)?;
    if ent.name.as_slice() == target {
        *idx += 1;
        Some(ent)
    } else {
        None
    }
}

fn join_rel_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut out = Vec::with_capacity(prefix.len() + 1 + name.len());
        out.extend_from_slice(prefix);
        out.push(b'/');
        out.extend_from_slice(name);
        out
    }
}

/// Parameters for constructing a first-parent merge snapshot carrying
/// `Spawn-Head` and task provenance trailers (`#113` Comment #5).
#[derive(Debug, Clone)]
pub struct MergeSnapshotSpec<'a> {
    /// First-parent mainline snapshot digest.
    pub main_snapshot: &'a Digest,
    /// Second-parent branch snapshot digest.
    pub branch_snapshot: &'a Digest,
    /// Canonical `spawn-tree-v1` root digest produced by [`merge_trees`].
    pub merged_tree: &'a Digest,
    /// Optional precomputed 40-hex OID of `main_snapshot` (computed from CAS if `None`).
    pub main_git_oid: Option<GitOid>,
    /// Optional precomputed 40-hex OID of `branch_snapshot` (used for `Spawn-Head`; computed from CAS if `None`).
    pub branch_git_oid: Option<GitOid>,
    /// Git ident line without `author ` prefix (e.g. `"Name <email> 1700000000 +0000"`).
    pub author_line: &'a str,
    /// Git ident line without `committer ` prefix.
    pub committer_line: &'a str,
    /// Commit summary/body (trailers appended cleanly after blank line).
    pub message: &'a str,
    /// Optional `Spawn-Task` numeric trailer.
    pub spawn_task: Option<i64>,
    /// Optional `Spawn-Objective` numeric trailer.
    pub spawn_objective: Option<i64>,
    /// Additional `(Key, Value)` trailers emitted after `Spawn-Head`.
    pub extra_trailers: &'a [(String, String)],
}

/// Constructed first-parent merge snapshot persisted in `spawn-cas`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltMergeSnapshot {
    /// Canonical `spawn-snapshot-v1` SHA-256 digest.
    pub snapshot_id: Digest,
    /// Canonical snapshot object (`parents = [main_snapshot, branch_snapshot]`).
    pub snapshot: Snapshot,
    /// Projected deterministic 40-hex commit OID.
    pub git_oid: GitOid,
    /// Projected deterministic 40-hex root tree OID.
    pub tree_git_oid: GitOid,
}

/// Construct and store a first-parent merge `spawn-snapshot-v1` (`parents = [main, branch]`)
/// carrying `Spawn-Task`, optional `Spawn-Objective`, and `Spawn-Head: <branch 40-hex>`.
pub fn build_merge_snapshot(
    store: &CasMerkleStore,
    spec: &MergeSnapshotSpec<'_>,
) -> Result<BuiltMergeSnapshot, D6StoreError> {
    let mut cache = HashMap::new();
    let main_oid = match spec.main_git_oid {
        Some(oid) => oid,
        None => snapshot_oid_cached(store, spec.main_snapshot, &mut cache)?,
    };
    let branch_oid = match spec.branch_git_oid {
        Some(oid) => oid,
        None => snapshot_oid_cached(store, spec.branch_snapshot, &mut cache)?,
    };

    let tombstone = format!("{LARGE_FILE_TRAILER}:");
    if spec.message.lines().any(|line| {
        let t = line.trim_start();
        t.len() >= tombstone.len() && t[..tombstone.len()].eq_ignore_ascii_case(&tombstone)
    }) || spec
        .extra_trailers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case(LARGE_FILE_TRAILER))
    {
        return Err(D6StoreError::InvalidInput(format!(
            "tombstoned {LARGE_FILE_TRAILER} trailer is forbidden in D6 merge snapshots"
        )));
    }

    let mut full_message = spec.message.trim_end_matches('\n').to_string();
    full_message.push_str("\n\n");
    if let Some(task) = spec.spawn_task {
        let _ = writeln!(full_message, "Spawn-Task: {task}");
    }
    if let Some(obj) = spec.spawn_objective {
        let _ = writeln!(full_message, "Spawn-Objective: {obj}");
    }
    let _ = writeln!(full_message, "Spawn-Head: {branch_oid}");
    for (k, v) in spec.extra_trailers {
        if k.contains('\n') || v.contains('\n') {
            return Err(D6StoreError::InvalidInput(
                "trailer key/value cannot contain newlines".to_string(),
            ));
        }
        let _ = writeln!(full_message, "{k}: {v}");
    }

    let commit_body = format_commit_body(spec.author_line, spec.committer_line, &full_message);
    let snapshot = Snapshot {
        tree: spec.merged_tree.clone(),
        parents: vec![spec.main_snapshot.clone(), spec.branch_snapshot.clone()],
        commit_body,
    };
    let snapshot_id = store.cas().put_merkle_snapshot(&snapshot)?;
    let tree_git_oid = store.project_tree_oid(spec.merged_tree)?;
    let git_oid = store.project_snapshot_oid(&snapshot, &[main_oid, branch_oid])?;

    Ok(BuiltMergeSnapshot {
        snapshot_id,
        snapshot,
        git_oid,
        tree_git_oid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_edit_hunks(a: &[u32], b: &[u32], hunks: &[EditHunk]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut pos = 0;
        for h in hunks {
            out.extend_from_slice(&a[pos..h.a_start]);
            out.extend_from_slice(&b[h.b_start..h.b_end]);
            pos = h.a_end;
        }
        out.extend_from_slice(&a[pos..]);
        out
    }

    #[test]
    fn myers_diff_and_hunk_compaction_preserve_exact_patch_reconstruction() {
        let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
            (vec![], vec![]),
            (vec![], vec![1, 2, 3]),
            (vec![1, 2, 3], vec![]),
            (vec![1], vec![2]),
            (vec![1, 2, 3], vec![1, 2, 3]),
            (vec![1, 2, 3, 4, 5], vec![1, 9, 3, 8, 5]),
            (vec![1, 2, 1, 2, 1], vec![2, 1, 2, 1, 2]),
            (vec![1, 2, 3, 4], vec![4, 3, 2, 1]),
            (
                vec![10, 20, 30, 10, 20, 40, 50],
                vec![10, 20, 99, 10, 20, 30, 10, 20, 40],
            ),
        ];
        for (a, b) in cases {
            let hunks = myers_diff_hunks(&a, &b);
            assert_eq!(apply_edit_hunks(&a, &b, &hunks), b);
            for w in hunks.windows(2) {
                assert!(w[0].a_end < w[1].a_start || w[0].b_end < w[1].b_start);
            }
        }

        // Deterministic PRNG property fuzz across 300 random sequences with heavy repeats.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..300 {
            let mut next = || -> u64 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let len_a = usize::try_from(next() % 45).unwrap_or(0);
            let len_b = usize::try_from(next() % 45).unwrap_or(0);
            let alphabet = (next() % 6) + 1;
            let a: Vec<u32> = (0..len_a)
                .map(|_| u32::try_from(next() % alphabet).unwrap_or(0))
                .collect();
            let b: Vec<u32> = (0..len_b)
                .map(|_| u32::try_from(next() % alphabet).unwrap_or(0))
                .collect();
            let hunks = myers_diff_hunks(&a, &b);
            assert_eq!(apply_edit_hunks(&a, &b, &hunks), b);
        }
    }

    #[test]
    fn gitattributes_parser_respects_binary_macro_overrides_and_marker_size() {
        let raw = "\
[attr]custombin binary -crlf\n\
*.bin custombin\n\
*.dat binary merge=text\n\
*.log merge=union conflict-marker-size=11\n\
\"sub dir/file with space.c\" -text\n\
sub/** merge=customdrv\n\
";
        let attrs = GitAttributes::from_root_str(raw, &["sub/special.rs merge=text".to_string()])
            .expect("parse gitattributes");

        let bin = attrs.resolve_path(b"assets/foo.bin");
        assert!(bin.binary);
        assert_eq!(bin.merge, MergeDriver::Binary);
        assert_eq!(bin.text, TextAttr::Unset);

        let override_dat = attrs.resolve_path(b"x.dat");
        assert!(override_dat.binary);
        assert_eq!(override_dat.merge, MergeDriver::Text);
        assert_eq!(override_dat.text, TextAttr::Unset);

        let log = attrs.resolve_path(b"var/out.log");
        assert_eq!(log.merge, MergeDriver::Union);
        assert_eq!(log.conflict_marker_size, 11);

        let quoted = attrs.resolve_path(b"sub dir/file with space.c");
        assert_eq!(quoted.text, TextAttr::Unset);
        assert_eq!(quoted.merge, MergeDriver::Text);

        let custom = attrs.resolve_path(b"sub/a/b.rs");
        assert_eq!(custom.merge, MergeDriver::Custom("customdrv".to_string()));

        let overridden = attrs.resolve_path(b"sub/special.rs");
        assert_eq!(overridden.merge, MergeDriver::Text);
    }

    #[test]
    fn shadow_merge_classifier_implements_comment5_admission_rules() {
        let oid_a = GitOid::parse_hex(&"a".repeat(40)).expect("oid a");
        let oid_b = GitOid::parse_hex(&"b".repeat(40)).expect("oid b");
        let clean_report = MergeReport {
            tree: empty_tree_digest(),
            tree_git_oid: oid_a,
            conflicts: Vec::new(),
            subtrees_reused: 1,
            exact_renames_applied: 0,
        };
        let conflict_report = MergeReport {
            tree: empty_tree_digest(),
            tree_git_oid: oid_b,
            conflicts: vec![MergeConflict {
                path: TreePath::from("f.rs"),
                kind: ConflictKind::Content,
                detail: "conflict".to_string(),
            }],
            subtrees_reused: 0,
            exact_renames_applied: 0,
        };

        assert_eq!(
            classify_shadow_merge(&clean_report, true, Some(oid_a), 0, false),
            MergeShadowOutcome::AgreeClean { tree_oid: oid_a }
        );
        assert!(classify_shadow_merge(&clean_report, true, Some(oid_b), 0, false).is_blocker());
        assert!(classify_shadow_merge(&clean_report, false, Some(oid_b), 1, false).is_blocker());
        assert_eq!(
            classify_shadow_merge(&conflict_report, false, Some(oid_b), 1, false),
            MergeShadowOutcome::AgreeConflict {
                rust_conflicts: 1,
                git_conflict_paths: 1,
            }
        );
        assert!(classify_shadow_merge(&conflict_report, true, Some(oid_b), 0, false).is_blocker());
        assert_eq!(
            classify_shadow_merge(&conflict_report, true, Some(oid_b), 0, true),
            MergeShadowOutcome::ObservedRustConflictGitCleanNonExactRename {
                rust_conflicts: 1,
                git_tree_oid: oid_b,
            }
        );
    }

    #[test]
    fn upward_coalescing_reunites_split_insertion_matching_repeated_attribute_line() {
        let base_bytes = b"fn before() {}\n#[test]\nfn existing() {}\n";
        let side_bytes =
            b"fn before() {}\n/// doc\n#[test]\nfn added() {}\n#[test]\nfn existing() {}\n";
        let base_lines = split_lines_keep_newline(base_bytes);
        let side_lines = split_lines_keep_newline(side_bytes);
        let mut intern = HashMap::new();
        let base_ids = intern_lines(&base_lines, &mut intern);
        let side_ids = intern_lines(&side_lines, &mut intern);

        // Simulate middle-snake splitting across the repeated `#[test]` line.
        let split_raw = vec![
            EditHunk {
                a_start: 1,
                a_end: 1,
                b_start: 1,
                b_end: 2,
            },
            EditHunk {
                a_start: 2,
                a_end: 2,
                b_start: 3,
                b_end: 5,
            },
        ];
        let compacted = coalesce_and_compact_hunks(&base_ids, &side_ids, split_raw);
        assert_eq!(
            compacted,
            vec![EditHunk {
                a_start: 1,
                a_end: 1,
                b_start: 1,
                b_end: 4,
            }]
        );
        assert_eq!(apply_edit_hunks(&base_ids, &side_ids, &compacted), side_ids);

        // Both branches adding the same documented test produce identical single-hunk diffs.
        let main_hunks = myers_diff_hunks(&base_ids, &side_ids);
        let branch_hunks = myers_diff_hunks(&base_ids, &side_ids);
        assert_eq!(main_hunks, compacted);
        assert_eq!(main_hunks, branch_hunks);
    }
}
