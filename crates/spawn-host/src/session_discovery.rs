//! Zero-hardcoded native agent session auto-discovery & bounded tail reading (`Wave-AR AR-4A`).
//!
//! Discovers active session/transcript files for live agent PTYs without hardcoding a
//! single engine directory layout or reading ambient process environment variables:
//! - **Tier 1 (`ProcessOpenFd`)**: reflects live `/proc/<pid>/fd/*` open file descriptors
//!   across the PTY process tree and verifies content with [`sniff_session_metadata`].
//! - **Tier 2 (`ProcessEnvAndCwd`)**: reads target process `/proc/<pid>/cwd` and
//!   **allowlisted non-secret path variables only** from `/proc/<pid>/environ`
//!   (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
//!   `XDG_CONFIG_HOME`, `OPENCODE_HOME`, `GEMINI_CLI_HOME`, `SPAWN_AGY_HOME`, `HOME`)
//!   and scans bounded nested directories (handling Codex `YYYY/MM/DD/rollout-*.jsonl`,
//!   Claude `projects/<ws>/<id>.jsonl`, Agy `transcript_full.jsonl`, OpenCode, and generic JSONL).
//! - **Tier 3 (`DriverHint`)**: bounded scan across caller-supplied candidate roots.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spawn_core::agent_events::{
    AgentEngine, DiscoveryConfidence, SessionAttachment, SessionDiscoveryMethod,
    SessionDiscoveryStatus, SessionFileFormat, SniffedSessionMeta, sniff_session_metadata,
};

use crate::herdr_witness::collect_process_tree;

/// Hard cap on directory traversal depth below any candidate session root.
const MAX_SCAN_DEPTH: usize = 8;

/// Hard cap on candidate files stat'd/sniffed per discovery invocation.
const MAX_SCAN_FILES: usize = 400;

/// Soft byte threshold across head lines inspected during schema sniffing.
const SNIFF_HEAD_BYTES: usize = 256 * 1024;

/// Maximum head non-empty lines inspected during schema sniffing.
const SNIFF_HEAD_LINES: usize = 48;

/// Hard upper bound (`2 MiB`) for bounded session tail reads.
pub const MAX_SESSION_READ_BYTES: usize = 2 * 1024 * 1024;

/// Grace slack before process start time for recently touched session logs.
const START_MTIME_SLACK: Duration = Duration::from_secs(5);

/// Strict allowlist of non-secret directory environment keys inspected on target processes.
/// Every other env key (credentials, API keys, DB URLs, tokens) is discarded immediately.
const ALLOWED_ENV_DIR_KEYS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "OPENCODE_HOME",
    "GEMINI_CLI_HOME",
    "SPAWN_AGY_HOME",
    "HOME",
];

const IGNORED_PATH_PREFIXES: &[&str] = &[
    "/dev/", "/proc/", "/sys/", "/usr/", "/lib/", "/lib64/", "/etc/", "/run/",
];

const IGNORED_DIR_COMPONENTS: &[&str] =
    &[".git", "node_modules", "target", ".cargo", ".pnpm-store"];

const IGNORED_FILE_NAMES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "tsconfig.json",
    "jsconfig.json",
    "settings.json",
    "settings.local.json",
    "config.json",
    "auth.json",
    ".claude.json",
    "Cargo.toml",
    "Cargo.lock",
];

#[derive(Debug, Clone)]
struct CandidateFile {
    path: PathBuf,
    mtime: SystemTime,
}

/// Rich session discovery outcome including explicit diagnostics and file metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiscoveryOutcome {
    /// Verified session log attachment if discovered.
    pub attachment: Option<SessionAttachment>,
    /// Closed diagnostic status describing attachment or miss reason.
    pub status: SessionDiscoveryStatus,
    /// Size in bytes of the attached session file when stat succeeded.
    pub file_bytes: Option<u64>,
    /// Modification time of the attached session file in UTC milliseconds when stat succeeded.
    pub file_mtime_ms: Option<i64>,
}

/// Discover a native session/transcript log for a live terminal rooted at `leader_pid`,
/// returning structured attachment plus explicit discovery diagnostics and file stat metadata.
#[must_use]
pub fn discover_session_with_diagnostic(
    leader_pid: u32,
    engine_hint: Option<&AgentEngine>,
    explicit_roots: &[PathBuf],
) -> SessionDiscoveryOutcome {
    let pids = collect_process_tree(leader_pid);
    if pids.is_empty() {
        if !explicit_roots.is_empty() {
            return match discover_session_in_roots(
                explicit_roots,
                None,
                None,
                engine_hint,
                SessionDiscoveryMethod::DriverHint,
            ) {
                Some(att) => attached_outcome(att),
                None => no_match_outcome(explicit_roots.len()),
            };
        }
        return SessionDiscoveryOutcome {
            attachment: None,
            status: SessionDiscoveryStatus::NoLivePid,
            file_bytes: None,
            file_mtime_ms: None,
        };
    }

    if let Some(open_fd) = discover_via_open_fds(&pids, engine_hint) {
        return attached_outcome(open_fd);
    }

    let (cwd, env_roots, min_mtime) = collect_proc_context(&pids);
    let mut roots_checked = usize::from(!pids.is_empty()) + env_roots.len();
    if !env_roots.is_empty()
        && let Some(found) = discover_session_in_roots(
            &env_roots,
            cwd.as_deref(),
            min_mtime,
            engine_hint,
            SessionDiscoveryMethod::ProcessEnvAndCwd,
        )
    {
        return attached_outcome(found);
    }

    roots_checked += explicit_roots.len();
    if !explicit_roots.is_empty()
        && let Some(found) = discover_session_in_roots(
            explicit_roots,
            cwd.as_deref(),
            min_mtime,
            engine_hint,
            SessionDiscoveryMethod::DriverHint,
        )
    {
        return attached_outcome(found);
    }

    no_match_outcome(roots_checked)
}

/// Discover a native session/transcript log for a live terminal rooted at `leader_pid`,
/// checking open FDs first, then process `cwd` + allowlisted env roots, then `explicit_roots`.
#[must_use]
pub fn discover_session_for_pid(
    leader_pid: u32,
    engine_hint: Option<&AgentEngine>,
    explicit_roots: &[PathBuf],
) -> Option<SessionAttachment> {
    discover_session_with_diagnostic(leader_pid, engine_hint, explicit_roots).attachment
}

/// Scan `roots` (depth <= 5, symlinks skipped) for the best matching session file
/// verified by content sniffing (`sniff_session_metadata`).
#[must_use]
pub fn discover_session_in_roots(
    roots: &[PathBuf],
    target_cwd: Option<&Path>,
    min_mtime: Option<SystemTime>,
    engine_hint: Option<&AgentEngine>,
    method: SessionDiscoveryMethod,
) -> Option<SessionAttachment> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        if files.len() >= MAX_SCAN_FILES {
            break;
        }
        collect_candidate_files(root, 0, &mut seen, &mut files);
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.mtime));

    let norm_cwd = target_cwd.map(normalize_lexical);
    let mut best: Option<(u8, SystemTime, SessionAttachment)> = None;

    for cand in files {
        if let Some(cutoff) = min_mtime
            && cand.mtime + START_MTIME_SLACK < cutoff
        {
            continue;
        }
        let Some(meta) = sniff_regular_file(&cand.path) else {
            continue;
        };
        if !format_matches_engine(meta.format, engine_hint) {
            continue;
        }
        let confidence = classify_confidence(&meta, norm_cwd.as_deref(), engine_hint);
        let Some(conf) = confidence else {
            continue;
        };
        let score = candidate_rank(meta.format, conf, &cand.path);
        let attachment = attachment_from_meta(&cand.path, meta, method, conf);
        let replace = best.as_ref().is_none_or(|(best_score, best_time, _)| {
            (score, cand.mtime) > (*best_score, *best_time)
        });
        if replace {
            best = Some((score, cand.mtime, attachment));
        }
    }
    best.map(|(_, _, att)| att)
}

/// Read up to the last `max_bytes` (capped at [`MAX_SESSION_READ_BYTES`]) complete UTF-8
/// lines from a regular session file without following symlinks.
pub fn read_session_tail_lines(path: &Path, max_bytes: usize) -> Result<Vec<String>, String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|err| format!("stat session file {}: {err}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!(
            "refusing non-regular or symlink session file {}",
            path.display()
        ));
    }
    let cap = max_bytes.clamp(1, MAX_SESSION_READ_BYTES) as u64;
    let mut file =
        File::open(path).map_err(|err| format!("open session file {}: {err}", path.display()))?;
    let len = meta.len();
    let truncated = len > cap;
    if truncated {
        file.seek(SeekFrom::Start(len - cap))
            .map_err(|err| format!("seek session file {}: {err}", path.display()))?;
    }
    let mut buf = Vec::with_capacity(usize::try_from(len.min(cap)).unwrap_or(65_536));
    file.take(cap)
        .read_to_end(&mut buf)
        .map_err(|err| format!("read session file {}: {err}", path.display()))?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        if let Some(pos) = text.find('\n') {
            text.drain(..=pos);
        } else {
            return Ok(Vec::new());
        }
    }
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn discover_via_open_fds(
    pids: &[u32],
    engine_hint: Option<&AgentEngine>,
) -> Option<SessionAttachment> {
    let mut seen_paths = BTreeSet::new();
    let mut best: Option<(u8, SystemTime, SessionAttachment)> = None;

    for &pid in pids {
        let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(raw_target) = fs::read_link(entry.path()) else {
                continue;
            };
            let target = prefer_agy_full_sibling(&raw_target);
            if !seen_paths.insert(target.clone()) || !is_candidate_session_path(&target) {
                continue;
            }
            let Ok(file_meta) = fs::symlink_metadata(&target) else {
                continue;
            };
            if file_meta.file_type().is_symlink() || !file_meta.is_file() || file_meta.len() == 0 {
                continue;
            }
            let Some(meta) = sniff_regular_file(&target) else {
                continue;
            };
            if !format_matches_engine(meta.format, engine_hint) {
                continue;
            }
            let mtime = file_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let score = candidate_rank(meta.format, DiscoveryConfidence::ExactFd, &target);
            let att = attachment_from_meta(
                &target,
                meta,
                SessionDiscoveryMethod::ProcessOpenFd,
                DiscoveryConfidence::ExactFd,
            );
            let replace = best
                .as_ref()
                .is_none_or(|(best_score, best_t, _)| (score, mtime) > (*best_score, *best_t));
            if replace {
                best = Some((score, mtime, att));
            }
        }
    }
    best.map(|(_, _, att)| att)
}

fn collect_proc_context(pids: &[u32]) -> (Option<PathBuf>, Vec<PathBuf>, Option<SystemTime>) {
    let mut cwd: Option<PathBuf> = None;
    let mut env_dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut earliest_start: Option<SystemTime> = None;

    for &pid in pids {
        if cwd.is_none()
            && let Ok(link) = fs::read_link(format!("/proc/{pid}/cwd"))
            && link.is_absolute()
        {
            cwd = Some(link);
        }
        if let Ok(bytes) = fs::read(format!("/proc/{pid}/environ")) {
            extract_allowlisted_env_dirs(&bytes, &mut env_dirs);
        }
        if let Ok(meta) = fs::symlink_metadata(format!("/proc/{pid}/stat"))
            && let Ok(created) = meta.modified()
        {
            earliest_start = Some(match earliest_start {
                None => created,
                Some(prev) => prev.min(created),
            });
        }
    }

    let roots = expand_env_candidate_roots(&env_dirs);
    (cwd, roots, earliest_start)
}

fn extract_allowlisted_env_dirs(bytes: &[u8], out: &mut BTreeMap<String, PathBuf>) {
    for pair in bytes.split(|&b| b == 0) {
        let Ok(text) = std::str::from_utf8(pair) else {
            continue;
        };
        let Some((key, val)) = text.split_once('=') else {
            continue;
        };
        if !ALLOWED_ENV_DIR_KEYS.contains(&key) {
            continue;
        }
        let trimmed = val.trim();
        let path = Path::new(trimmed);
        if !trimmed.is_empty() && path.is_absolute() && !out.contains_key(key) {
            out.insert(key.to_string(), path.to_path_buf());
        }
    }
}

fn expand_env_candidate_roots(env_dirs: &BTreeMap<String, PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push_existing = |path: PathBuf| {
        if path.is_dir() && !roots.contains(&path) {
            roots.push(path);
        }
    };

    if let Some(dir) = env_dirs.get("CODEX_HOME") {
        push_existing(dir.join("sessions"));
    }
    if let Some(dir) = env_dirs.get("CLAUDE_CONFIG_DIR") {
        push_existing(dir.join("projects"));
    }
    if let Some(dir) = env_dirs.get("SPAWN_AGY_HOME") {
        push_existing(dir.clone());
    }
    if let Some(dir) = env_dirs.get("OPENCODE_HOME") {
        push_existing(dir.clone());
    }
    if let Some(dir) = env_dirs.get("GEMINI_CLI_HOME") {
        push_existing(dir.clone());
    }
    if let Some(xdg_data) = env_dirs.get("XDG_DATA_HOME") {
        push_existing(xdg_data.join("opencode"));
    }
    if let Some(home) = env_dirs.get("HOME") {
        push_existing(home.join(".codex/sessions"));
        push_existing(home.join(".claude/projects"));
        push_existing(home.join(".gemini/antigravity-cli"));
        push_existing(home.join(".t3/userdata/providers/antigravity"));
        push_existing(home.join(".local/share/opencode"));
    }
    roots
}

fn collect_candidate_files(
    dir: &Path,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<CandidateFile>,
) {
    if depth > MAX_SCAN_DEPTH || out.len() >= MAX_SCAN_FILES {
        return;
    }
    let Ok(dir_meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        if out.len() >= MAX_SCAN_FILES {
            break;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !IGNORED_DIR_COMPONENTS.contains(&name) {
                subdirs.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), path));
            }
            continue;
        }
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        if is_candidate_session_path(&path) && seen.insert(path.clone()) {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push(CandidateFile { path, mtime });
        }
    }
    subdirs.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, sub) in subdirs {
        if out.len() >= MAX_SCAN_FILES {
            break;
        }
        collect_candidate_files(&sub, depth + 1, seen, out);
    }
}

fn prefer_agy_full_sibling(path: &Path) -> PathBuf {
    if path.file_name().and_then(|s| s.to_str()) == Some("transcript.jsonl") {
        let full = path.with_file_name("transcript_full.jsonl");
        if let Ok(meta) = fs::symlink_metadata(&full)
            && !meta.file_type().is_symlink()
            && meta.is_file()
            && meta.len() > 0
        {
            return full;
        }
    }
    path.to_path_buf()
}

fn is_candidate_session_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let text = path.to_string_lossy();
    if IGNORED_PATH_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return false;
    }
    for comp in path.components() {
        if let std::path::Component::Normal(os) = comp
            && let Some(name) = os.to_str()
            && IGNORED_DIR_COMPONENTS.contains(&name)
        {
            return false;
        }
    }
    let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if IGNORED_FILE_NAMES.contains(&file_name) {
        return false;
    }
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("json")
}

fn sniff_regular_file(path: &Path) -> Option<SniffedSessionMeta> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() == 0 {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file.take((SNIFF_HEAD_BYTES * 2) as u64));
    let mut owned_lines = Vec::new();
    let mut bytes_read = 0usize;
    while owned_lines.len() < SNIFF_HEAD_LINES && bytes_read < SNIFF_HEAD_BYTES {
        let mut line = String::new();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        bytes_read += n;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            owned_lines.push(trimmed.to_string());
        }
    }
    let refs: Vec<&str> = owned_lines.iter().map(String::as_str).collect();
    sniff_session_metadata(&refs)
}

fn is_safe_session_token(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 96
        && !raw.starts_with('.')
        && !raw.starts_with('-')
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn extract_session_id_from_path(path: &Path, format: SessionFileFormat) -> Option<String> {
    match format {
        SessionFileFormat::AgyFullJsonl => {
            let comps: Vec<&str> = path
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect();
            for window in comps.windows(3) {
                if window[0] == "brain"
                    && window[2] == ".system_generated"
                    && is_safe_session_token(window[1])
                {
                    return Some(window[1].to_string());
                }
            }
            None
        }
        SessionFileFormat::ClaudeJsonl => {
            let stem = path.file_stem()?.to_str()?;
            is_safe_session_token(stem).then(|| stem.to_string())
        }
        SessionFileFormat::CodexRolloutJsonl => {
            let stem = path.file_stem()?.to_str()?;
            if let Some(after) = stem.strip_prefix("rollout-") {
                let bytes = after.as_bytes();
                if bytes.len() > 20 && bytes[10] == b'T' && bytes[19] == b'-' {
                    let cand = &after[20..];
                    return is_safe_session_token(cand).then(|| cand.to_string());
                }
            }
            is_safe_session_token(stem).then(|| stem.to_string())
        }
        SessionFileFormat::OpenCodeJsonl | SessionFileFormat::GenericJsonl => None,
    }
}

fn attached_outcome(att: SessionAttachment) -> SessionDiscoveryOutcome {
    let (file_bytes, file_mtime_ms) = match fs::symlink_metadata(&att.path) {
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_file() => (
            Some(meta.len()),
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_millis()).ok()),
        ),
        _ => (None, None),
    };
    let status = SessionDiscoveryStatus::Attached {
        method: att.method,
        confidence: att.confidence,
    };
    SessionDiscoveryOutcome {
        attachment: Some(att),
        status,
        file_bytes,
        file_mtime_ms,
    }
}

const fn no_match_outcome(scanned_roots: usize) -> SessionDiscoveryOutcome {
    SessionDiscoveryOutcome {
        attachment: None,
        status: SessionDiscoveryStatus::NoMatchingSessionFile { scanned_roots },
        file_bytes: None,
        file_mtime_ms: None,
    }
}

fn format_matches_engine(format: SessionFileFormat, engine: Option<&AgentEngine>) -> bool {
    match engine {
        None | Some(AgentEngine::Generic(_)) => true,
        Some(AgentEngine::Claude) => {
            matches!(
                format,
                SessionFileFormat::ClaudeJsonl | SessionFileFormat::GenericJsonl
            )
        }
        Some(AgentEngine::Codex) => {
            matches!(
                format,
                SessionFileFormat::CodexRolloutJsonl | SessionFileFormat::GenericJsonl
            )
        }
        Some(AgentEngine::Agy) => {
            matches!(
                format,
                SessionFileFormat::AgyFullJsonl | SessionFileFormat::GenericJsonl
            )
        }
        Some(AgentEngine::OpenCode) => {
            matches!(
                format,
                SessionFileFormat::OpenCodeJsonl | SessionFileFormat::GenericJsonl
            )
        }
        Some(AgentEngine::Gemini) => {
            matches!(
                format,
                SessionFileFormat::AgyFullJsonl | SessionFileFormat::GenericJsonl
            )
        }
    }
}

fn classify_confidence(
    meta: &SniffedSessionMeta,
    norm_cwd: Option<&Path>,
    engine_hint: Option<&AgentEngine>,
) -> Option<DiscoveryConfidence> {
    if let (Some(want_cwd), Some(got_cwd)) = (norm_cwd, meta.cwd.as_deref()) {
        let got_norm = normalize_lexical(Path::new(got_cwd));
        if got_norm == want_cwd {
            return Some(DiscoveryConfidence::MatchedMetadata);
        }
        return None;
    }
    if engine_hint.is_some() && meta.format != SessionFileFormat::GenericJsonl {
        return Some(DiscoveryConfidence::HeuristicRecent);
    }
    if meta.native_session_id.is_some() {
        return Some(DiscoveryConfidence::HeuristicRecent);
    }
    None
}

fn candidate_rank(format: SessionFileFormat, confidence: DiscoveryConfidence, path: &Path) -> u8 {
    let conf_score = match confidence {
        DiscoveryConfidence::ExactFd => 60,
        DiscoveryConfidence::MatchedMetadata => 40,
        DiscoveryConfidence::HeuristicRecent => 20,
    };
    let fmt_score = match format {
        SessionFileFormat::GenericJsonl => 2,
        _ => 10,
    };
    let full_bonus = if path.file_name().and_then(|s| s.to_str()) == Some("transcript_full.jsonl") {
        5
    } else {
        0
    };
    conf_score + fmt_score + full_bonus
}

fn attachment_from_meta(
    path: &Path,
    meta: SniffedSessionMeta,
    method: SessionDiscoveryMethod,
    confidence: DiscoveryConfidence,
) -> SessionAttachment {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let native_session_id = meta
        .native_session_id
        .or_else(|| extract_session_id_from_path(&canonical, meta.format));
    SessionAttachment {
        path: canonical.display().to_string(),
        format: meta.format,
        method,
        confidence,
        native_session_id,
        cwd: meta.cwd,
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "spawn-session-disc-{label}-{}-{nanos}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("mkdir temp");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn live_open_fd_discovery_finds_session_without_any_path_hints() {
        let dir = TempDir::new("open-fd");
        let log_path = dir.0.join("custom-location-rollout.jsonl");
        let mut f = File::create(&log_path).expect("create session log");
        writeln!(
            f,
            r#"{{"ordinal":0,"timestamp":"2026-11-28T10:00:00Z","type":"session_meta","payload":{{"id":"fd-sess-1","cwd":"/tmp/ws"}}}}"#
        )
        .expect("write header");
        f.flush().expect("flush");

        // Keep `f` open in this process and discover via `/proc/<pid>/fd`.
        let discovered =
            discover_session_for_pid(std::process::id(), Some(&AgentEngine::Codex), &[])
                .expect("must discover open session fd");
        assert_eq!(discovered.method, SessionDiscoveryMethod::ProcessOpenFd);
        assert_eq!(discovered.confidence, DiscoveryConfidence::ExactFd);
        assert_eq!(discovered.format, SessionFileFormat::CodexRolloutJsonl);
        assert_eq!(discovered.native_session_id.as_deref(), Some("fd-sess-1"));
        assert_eq!(
            Path::new(&discovered.path),
            fs::canonicalize(&log_path).expect("canon")
        );
    }

    #[test]
    fn nested_codex_claude_agy_and_generic_discovery_matches_cwd_without_hardcoding() {
        let dir = TempDir::new("nested");
        let ws = dir.0.join("ws-target");
        let other_ws = dir.0.join("ws-other");
        fs::create_dir_all(&ws).expect("ws");
        fs::create_dir_all(&other_ws).expect("other_ws");

        let codex_nested = dir
            .0
            .join("codex-home/sessions/2026/11/28/rollout-2026-11-28T00-10-49-cx99.jsonl");
        fs::create_dir_all(codex_nested.parent().unwrap()).unwrap();
        fs::write(
            &codex_nested,
            format!(
                r#"{{"ordinal":0,"timestamp":"2026-11-28T00:10:49Z","type":"session_meta","payload":{{"id":"cx99","cwd":"{}"}}}}"#,
                ws.display()
            ),
        )
        .unwrap();

        let claude_other = dir.0.join("claude/projects/other/c1.jsonl");
        fs::create_dir_all(claude_other.parent().unwrap()).unwrap();
        fs::write(
            &claude_other,
            format!(
                r#"{{"type":"user","sessionId":"cl-other","cwd":"{}","timestamp":"2026-11-28T01:00:00Z","message":{{"role":"user","content":"hi"}}}}"#,
                other_ws.display()
            ),
        )
        .unwrap();

        // Noise package.json must never qualify as a session log.
        fs::write(
            dir.0.join("package.json"),
            r#"{"name":"ignored","version":"1.0.0"}"#,
        )
        .unwrap();

        let found_codex = discover_session_in_roots(
            &[dir.0.join("codex-home/sessions")],
            Some(&ws),
            None,
            Some(&AgentEngine::Codex),
            SessionDiscoveryMethod::ProcessEnvAndCwd,
        )
        .expect("nested codex rollout must match");
        assert_eq!(found_codex.format, SessionFileFormat::CodexRolloutJsonl);
        assert_eq!(found_codex.confidence, DiscoveryConfidence::MatchedMetadata);
        assert_eq!(found_codex.native_session_id.as_deref(), Some("cx99"));

        // Wrong workspace cwd does not cross-match onto the wrong agent's log.
        assert_eq!(
            discover_session_in_roots(
                &[dir.0.join("claude/projects")],
                Some(&ws),
                None,
                Some(&AgentEngine::Claude),
                SessionDiscoveryMethod::ProcessEnvAndCwd,
            ),
            None
        );

        let generic_file = dir.0.join("custom-cli/run-1.jsonl");
        fs::create_dir_all(generic_file.parent().unwrap()).unwrap();
        fs::write(
            &generic_file,
            format!(
                r#"{{"type":"assistant","role":"assistant","session_id":"gen-1","cwd":"{}","message":"ready"}}"#,
                ws.display()
            ),
        )
        .unwrap();
        let found_generic = discover_session_in_roots(
            &[dir.0.join("custom-cli")],
            Some(&ws),
            None,
            Some(&AgentEngine::OpenCode),
            SessionDiscoveryMethod::ProcessEnvAndCwd,
        )
        .expect("generic session file");
        assert_eq!(found_generic.format, SessionFileFormat::GenericJsonl);
    }

    #[test]
    fn allowlisted_env_extractor_drops_all_secret_variables() {
        let raw = b"HOME=/home/w\0CODEX_HOME=/opt/cx\0OPENAI_API_KEY=sk-secret\0SPAWN_DATABASE_URL=postgres://u:p@h/d\0";
        let mut out = BTreeMap::new();
        extract_allowlisted_env_dirs(raw, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out.get("HOME"), Some(&PathBuf::from("/home/w")));
        assert_eq!(out.get("CODEX_HOME"), Some(&PathBuf::from("/opt/cx")));
        assert!(!out.contains_key("OPENAI_API_KEY"));
        assert!(!out.contains_key("SPAWN_DATABASE_URL"));
    }

    #[test]
    fn tail_reader_handles_truncation_on_line_boundary() {
        use std::fmt::Write as _;

        let dir = TempDir::new("tail");
        let p = dir.0.join("lines.jsonl");
        let mut s = String::new();
        for i in 0..50 {
            let _ = writeln!(s, r#"{{"n":{i}}}"#);
        }
        fs::write(&p, &s).unwrap();
        let lines = read_session_tail_lines(&p, 64).expect("tail lines");
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        }
        assert_eq!(lines.last().unwrap(), "{\"n\":49}");
    }

    #[test]
    fn deep_agy_path_large_head_line_and_diagnostics_work_end_to_end() {
        let dir = TempDir::new("deep-agy-large");
        let uuid = "adb70585-37c5-473f-887a-c071e9b2ccf5";
        let deep_log = dir
            .0
            .join("providers/antigravity/hash123/antigravity-acp/brain")
            .join(uuid)
            .join(".system_generated/logs/transcript_full.jsonl");
        fs::create_dir_all(deep_log.parent().unwrap()).unwrap();
        fs::write(
            &deep_log,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"created_at\":\"2026-11-28T10:00:00Z\",\"content\":\"hi\"}\n",
        )
        .unwrap();

        let found_agy = discover_session_in_roots(
            &[dir.0.join("providers/antigravity")],
            None,
            None,
            Some(&AgentEngine::Agy),
            SessionDiscoveryMethod::ProcessEnvAndCwd,
        )
        .expect("depth-6 agy log must be found");
        assert_eq!(found_agy.format, SessionFileFormat::AgyFullJsonl);
        assert_eq!(found_agy.native_session_id.as_deref(), Some(uuid));

        // Large 50 KiB single line must sniff cleanly via complete-line BufReader without cutting JSON.
        let large_dir = TempDir::new("large-head");
        let large_file = large_dir.0.join("large-sess.jsonl");
        let big_pad = "x".repeat(50 * 1024);
        fs::write(
            &large_file,
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"cl-large\",\"cwd\":\"/tmp/ws\",\"timestamp\":\"2026-11-28T10:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"{big_pad}\"}}}}\n"
            ),
        )
        .unwrap();
        let meta = sniff_regular_file(&large_file).expect("50KiB first line must sniff");
        assert_eq!(meta.format, SessionFileFormat::ClaudeJsonl);
        assert_eq!(meta.native_session_id.as_deref(), Some("cl-large"));

        let empty_root = dir.0.join("empty-root");
        fs::create_dir_all(&empty_root).unwrap();
        let diag = discover_session_with_diagnostic(
            u32::MAX,
            Some(&AgentEngine::Codex),
            std::slice::from_ref(&empty_root),
        );
        assert_eq!(diag.attachment, None);
        assert!(matches!(
            diag.status,
            SessionDiscoveryStatus::NoMatchingSessionFile { scanned_roots: 1 }
        ));
    }
}
