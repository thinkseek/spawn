//! Host-side second witness for embedded `herdr-core` sessions.
//!
//! Combines three signals from outside any sandbox:
//! 1. Authoritative `/proc/<pid>` process-tree inspection rooted at the PTY leader
//!    (`detect_agent_in_tree`, preserving anti-forgery `N2`);
//! 2. Screen-buffer rule matching over `terminal.read` with account lines stripped
//!    (`drop_agy_account_lines`, preserving `N4`);
//! 3. Optional OSC 7501 status corroboration when the agent emits structured status.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;

use spawn_core::agent_events::{AgentEngine, ProcessNode, ProcessTreeTelemetry};
use spawn_core::port::{AgentReading, AgentStatus};
use spawn_core::screen::{
    ScreenState, drop_agy_account_lines, is_agy_trust_dialog, is_trust_dialog, read_agy_screen,
    read_claude_screen,
};

/// Host-decoded subset of `herdr-core` OSC 7501 agent status metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OscStatus {
    /// Reported agent activity state string (for example `busy`, `idle`, `waiting`).
    pub state: String,
    /// Optional structured wait or notification kind.
    pub kind: Option<String>,
    /// Optional numeric progress percentage (`0..=100`).
    pub progress: Option<u8>,
    /// Emitting application identifier.
    pub app: String,
    /// Optional status title shown alongside the agent state.
    pub title: String,
    /// Detailed status message body.
    pub msg: String,
}

impl OscStatus {
    /// Parses OSC 7501 status JSON returned in `terminal.get`'s `status` field.
    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Self> {
        let state = value.get("state")?.as_str()?.to_string();
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let progress = value
            .get("progress")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u8::try_from(v).ok());
        let app = value
            .get("app")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let title = value
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let msg = value
            .get("msg")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        Some(Self {
            state,
            kind,
            progress,
            app,
            title,
            msg,
        })
    }
}

/// Manifest version tag reported by the host-side `agy` witness.
const AGY_MANIFEST: &str = "host-witness:agy-2026.10.05.1";

/// Manifest version tag reported by the host-side Claude Code witness.
const CLAUDE_MANIFEST: &str = "host-witness:claude-1";

/// Manifest version tag reported by the host-side Codex CLI witness.
const CODEX_MANIFEST: &str = "host-witness:codex-1";

/// Manifest version tag reported by the host-side OpenCode CLI witness.
const OPENCODE_MANIFEST: &str = "host-witness:opencode-1";

/// Manifest version tag reported by the host-side Gemini CLI witness.
const GEMINI_MANIFEST: &str = "host-witness:gemini-1";

/// Manifest version tag reported by the host-side generic CLI witness.
const GENERIC_MANIFEST: &str = "host-witness:generic-1";

/// Interpreters whose first non-flag script argument names the executed agent
/// (when not invoked via `-c` / `-e` inline shell snippets).
const INTERPRETERS: [&str; 11] = [
    "sh", "bash", "dash", "zsh", "env", "node", "nodejs", "bun", "deno", "python", "python3",
];

fn engine_manifest(engine: &spawn_core::agent_events::AgentEngine) -> &'static str {
    use spawn_core::agent_events::AgentEngine;
    match engine {
        AgentEngine::Agy => AGY_MANIFEST,
        AgentEngine::Claude => CLAUDE_MANIFEST,
        AgentEngine::Codex => CODEX_MANIFEST,
        AgentEngine::OpenCode => OPENCODE_MANIFEST,
        AgentEngine::Gemini => GEMINI_MANIFEST,
        AgentEngine::Generic(_) => GENERIC_MANIFEST,
    }
}

/// Reading returned when no supported coding agent runs in the terminal.
pub(crate) fn no_agent() -> AgentReading {
    AgentReading {
        agent: None,
        status: AgentStatus::Unknown,
        rule: None,
        blocker_visible: false,
        session: None,
        seq: 0,
        manifest: None,
    }
}

/// Identifies whether `comm` / `argv` corresponds to a supported agent process.
///
/// Inline shell snippets (`sh -c "agy ..."`) are explicitly ignored so a plain shell
/// running commands named after an agent never counts as an agent (`N2`).
fn classify_process(comm: &str, argv: &[String]) -> Option<spawn_core::agent_events::AgentEngine> {
    if let Some(agent) = match_binary_name(comm) {
        return Some(agent);
    }
    let first = argv.first()?;
    if let Some(agent) = path_basename(first).and_then(match_binary_name) {
        return Some(agent);
    }
    let base = path_basename(first)?;
    if INTERPRETERS.contains(&base) {
        return interpreter_target(argv).and_then(match_binary_name);
    }
    None
}

fn match_binary_name(name: &str) -> Option<spawn_core::agent_events::AgentEngine> {
    spawn_core::agent_events::AgentEngine::from_binary_name(name)
}

fn path_basename(arg: &str) -> Option<&str> {
    if arg.is_empty() {
        return None;
    }
    Path::new(arg).file_name().and_then(|s| s.to_str())
}

fn interpreter_target(argv: &[String]) -> Option<&str> {
    for arg in argv.iter().skip(1) {
        if arg == "--" {
            continue;
        }
        if arg.starts_with('-') {
            if matches!(arg.as_str(), "-c" | "-e" | "--eval")
                || arg.chars().skip(1).any(|c| c == 'c')
            {
                return None;
            }
            continue;
        }
        if arg.contains('=') && !arg.contains('/') {
            continue;
        }
        return path_basename(arg);
    }
    None
}

/// Point-in-time snapshot of live `/proc` PIDs, parent links, and states
/// so callers inspecting multiple terminals only walk `/proc` once per tick.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcSnapshot {
    children: BTreeMap<u32, Vec<u32>>,
    stats: BTreeMap<u32, (char, u32)>,
}

impl ProcSnapshot {
    /// Reads `/proc` once and indexes all non-zombie PIDs.
    pub(crate) fn capture() -> Self {
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut stats: BTreeMap<u32, (char, u32)> = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Self::default();
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some((state, ppid)) = read_proc_stat(pid)
                && state != 'Z'
            {
                stats.insert(pid, (state, ppid));
                children.entry(ppid).or_default().push(pid);
            }
        }
        Self { children, stats }
    }

    /// Returns all live PIDs rooted at `leader_pid` in BFS order.
    pub(crate) fn tree_pids(&self, leader_pid: u32) -> Vec<u32> {
        if leader_pid <= 1 || !self.stats.contains_key(&leader_pid) {
            return Vec::new();
        }
        let mut order = Vec::new();
        let mut queue = VecDeque::from([leader_pid]);
        let mut seen = HashSet::from([leader_pid]);
        while let Some(pid) = queue.pop_front() {
            order.push(pid);
            if let Some(kids) = self.children.get(&pid) {
                for &kid in kids {
                    if seen.insert(kid) {
                        queue.push_back(kid);
                    }
                }
            }
        }
        order
    }
}

/// Walks the live `/proc` process tree rooted at `leader_pid` in BFS order,
/// returning all live (non-zombie) PIDs starting with `leader_pid`.
pub(crate) fn collect_process_tree(leader_pid: u32) -> Vec<u32> {
    ProcSnapshot::capture().tree_pids(leader_pid)
}

const MAX_PROCESS_TREE_NODES: usize = 32;
const MAX_CMDLINE_CHARS: usize = 160;
const IGNORE_LEAF_COMMS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "sleep", "cat", "tail", "env", "timeout",
];

fn detect_agent_in_pids(pids: &[u32]) -> Option<AgentEngine> {
    pids.iter().copied().find_map(inspect_pid)
}

/// Samples up to 32 live `/proc` nodes rooted at `leader_pid` using a captured `ProcSnapshot`.
pub(crate) fn inspect_process_tree_in(
    snapshot: &ProcSnapshot,
    leader_pid: u32,
) -> Option<ProcessTreeTelemetry> {
    let pids = snapshot.tree_pids(leader_pid);
    if pids.is_empty() {
        return None;
    }
    let mut processes = Vec::new();
    let mut argvs = Vec::new();
    for pid in pids.into_iter().take(MAX_PROCESS_TREE_NODES) {
        let Some(&(state, ppid)) = snapshot.stats.get(&pid) else {
            continue;
        };
        let (comm, argv) = read_proc_comm_argv(pid).unwrap_or_default();
        let command = format_sanitized_command(&comm, &argv);
        let rss_kib = read_proc_rss_kib(pid);
        argvs.push(argv);
        processes.push(ProcessNode {
            pid,
            ppid,
            comm,
            state: state.to_string(),
            rss_kib,
            command,
        });
    }
    if processes.is_empty() {
        return None;
    }
    let total_rss_kib = processes.iter().map(|p| p.rss_kib).sum();
    let active_leaf_command = pick_active_leaf_command(leader_pid, &processes, &argvs);
    let process_count = processes.len();
    Some(ProcessTreeTelemetry {
        leader_pid,
        process_count,
        child_count: process_count.saturating_sub(1),
        total_rss_kib,
        active_leaf_command,
        processes,
    })
}

fn pick_active_leaf_command(
    leader_pid: u32,
    processes: &[spawn_core::agent_events::ProcessNode],
    argvs: &[Vec<String>],
) -> Option<String> {
    for (node, argv) in processes.iter().zip(argvs.iter()).rev() {
        if node.pid != leader_pid
            && !IGNORE_LEAF_COMMS.contains(&node.comm.as_str())
            && classify_process(&node.comm, argv).is_none()
            && !node.command.is_empty()
        {
            return Some(node.command.clone());
        }
    }
    for (node, argv) in processes.iter().zip(argvs.iter()).rev() {
        if classify_process(&node.comm, argv).is_some() && !node.command.is_empty() {
            return Some(node.command.clone());
        }
    }
    None
}

fn read_proc_stat(pid: u32) -> Option<(char, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent = fields.next()?.parse::<u32>().ok()?;
    Some((state, parent))
}

fn read_proc_rss_kib(pid: u32) -> u64 {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|num| num.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    0
}

fn format_sanitized_command(comm: &str, argv: &[String]) -> String {
    let raw = if argv.is_empty() {
        comm.to_string()
    } else {
        argv.join(" ")
    };
    let redacted = spawn_core::redact::redact(&raw);
    let collapsed = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (idx, ch) in collapsed.chars().enumerate() {
        if idx >= MAX_CMDLINE_CHARS {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn read_proc_comm_argv(pid: u32) -> Option<(String, Vec<String>)> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim_end_matches(&['\n', '\r'][..]).to_string())
        .unwrap_or_default();
    let cmdline_bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = cmdline_bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    Some((comm, argv))
}

fn inspect_pid(pid: u32) -> Option<spawn_core::agent_events::AgentEngine> {
    let (comm, argv) = read_proc_comm_argv(pid)?;
    classify_process(&comm, &argv)
}

/// Combines `/proc` process verification (from `snapshot`) with screen and optional OSC 7501.
pub(crate) fn observe_in(
    snapshot: &ProcSnapshot,
    leader_pid: Option<u32>,
    raw_screen: &str,
    osc_status: Option<&OscStatus>,
) -> AgentReading {
    let Some(pid) = leader_pid else {
        return no_agent();
    };
    let pids = snapshot.tree_pids(pid);
    let Some(agent) = detect_agent_in_pids(&pids) else {
        return no_agent();
    };
    witness_screen(&agent, raw_screen, osc_status)
}

/// Combines `/proc` process verification with screen and optional OSC 7501 observation.
pub(crate) fn observe(
    leader_pid: Option<u32>,
    raw_screen: &str,
    osc_status: Option<&OscStatus>,
) -> AgentReading {
    observe_in(&ProcSnapshot::capture(), leader_pid, raw_screen, osc_status)
}

/// Evaluates `raw_screen` and optional `osc_status` for a verified `agent`.
fn witness_screen(
    agent: &spawn_core::agent_events::AgentEngine,
    raw_screen: &str,
    osc_status: Option<&OscStatus>,
) -> AgentReading {
    use spawn_core::agent_events::AgentEngine;
    let clean = drop_agy_account_lines(raw_screen);
    let (status, rule, blocker_visible) = match agent {
        AgentEngine::Agy => witness_agy(&clean, osc_status),
        AgentEngine::Claude => witness_claude(&clean, osc_status),
        AgentEngine::Codex => witness_codex(&clean, osc_status),
        AgentEngine::OpenCode | AgentEngine::Gemini | AgentEngine::Generic(_) => {
            witness_generic_osc(&clean, osc_status)
        }
    };
    AgentReading {
        agent: Some(agent.as_str().to_string()),
        status,
        rule: rule.map(str::to_string),
        blocker_visible,
        session: None,
        seq: 0,
        manifest: Some(engine_manifest(agent).to_string()),
    }
}

fn witness_agy(
    clean: &str,
    osc_status: Option<&OscStatus>,
) -> (AgentStatus, Option<&'static str>, bool) {
    if is_agy_trust_dialog(clean) {
        return (AgentStatus::Blocked, Some("trust_prompt"), true);
    }
    let non_empty: Vec<&str> = clean
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let bottom = non_empty[non_empty.len().saturating_sub(2)..]
        .join("\n")
        .to_ascii_lowercase();
    if bottom.contains("navigate") && bottom.contains("tab amend") {
        return (AgentStatus::Blocked, Some("permission_prompt"), true);
    }
    if bottom.contains("navigate") && bottom.contains("esc skip") {
        return (AgentStatus::Blocked, Some("question_prompt"), true);
    }

    let screen = read_agy_screen(clean);
    match screen.state {
        ScreenState::Permission => (AgentStatus::Blocked, Some("permission_prompt"), true),
        ScreenState::Busy => {
            if screen.evidence.contains("? for shortcuts") {
                if let Some((AgentStatus::Working, rule, false)) = osc_reading(osc_status) {
                    (AgentStatus::Working, rule, false)
                } else {
                    (AgentStatus::Idle, None, false)
                }
            } else {
                (
                    AgentStatus::Working,
                    Some("esc_cancel_footer_working"),
                    false,
                )
            }
        }
        ScreenState::Ready | ScreenState::Drafted => {
            if let Some((status, rule, blocker)) = osc_reading(osc_status)
                && matches!(status, AgentStatus::Working | AgentStatus::Blocked)
            {
                (status, rule, blocker)
            } else {
                (AgentStatus::Idle, None, false)
            }
        }
        ScreenState::Error | ScreenState::Unknown => {
            osc_reading(osc_status).unwrap_or((AgentStatus::Unknown, None, false))
        }
    }
}

fn witness_claude(
    clean: &str,
    osc_status: Option<&OscStatus>,
) -> (AgentStatus, Option<&'static str>, bool) {
    if is_trust_dialog(clean) {
        return (AgentStatus::Blocked, Some("trust_prompt"), true);
    }
    let screen = read_claude_screen(clean);
    match screen.state {
        ScreenState::Permission => (AgentStatus::Blocked, Some("permission_prompt"), true),
        ScreenState::Busy => (AgentStatus::Working, Some("claude_busy"), false),
        ScreenState::Ready | ScreenState::Drafted | ScreenState::Error => {
            if let Some((status, rule, blocker)) = osc_reading(osc_status)
                && matches!(status, AgentStatus::Working | AgentStatus::Blocked)
            {
                (status, rule, blocker)
            } else {
                (AgentStatus::Idle, None, false)
            }
        }
        ScreenState::Unknown => {
            osc_reading(osc_status).unwrap_or((AgentStatus::Unknown, None, false))
        }
    }
}

fn witness_codex(
    clean: &str,
    osc_status: Option<&OscStatus>,
) -> (AgentStatus, Option<&'static str>, bool) {
    if spawn_core::driver_codex::is_codex_trust_dialog(clean) || is_trust_dialog(clean) {
        return (AgentStatus::Blocked, Some("trust_prompt"), true);
    }
    let screen = spawn_core::driver_codex::read_codex_screen(clean);
    match screen.state {
        ScreenState::Permission => (AgentStatus::Blocked, Some("permission_prompt"), true),
        ScreenState::Busy => (AgentStatus::Working, Some("codex_busy"), false),
        ScreenState::Ready | ScreenState::Drafted | ScreenState::Error => {
            if let Some((status, rule, blocker)) = osc_reading(osc_status)
                && matches!(status, AgentStatus::Working | AgentStatus::Blocked)
            {
                (status, rule, blocker)
            } else {
                (AgentStatus::Idle, None, false)
            }
        }
        ScreenState::Unknown => {
            osc_reading(osc_status).unwrap_or((AgentStatus::Unknown, None, false))
        }
    }
}

fn witness_generic_osc(
    clean: &str,
    osc_status: Option<&OscStatus>,
) -> (AgentStatus, Option<&'static str>, bool) {
    if is_trust_dialog(clean) {
        return (AgentStatus::Blocked, Some("trust_prompt"), true);
    }
    if let Some(osc) = osc_reading(osc_status) {
        return osc;
    }
    if spawn_core::screen::is_choice_footer(clean) {
        return (AgentStatus::Blocked, Some("permission_prompt"), true);
    }
    (AgentStatus::Unknown, None, false)
}

fn osc_reading(
    osc_status: Option<&OscStatus>,
) -> Option<(AgentStatus, Option<&'static str>, bool)> {
    let status = osc_status?;
    match status.state.as_str() {
        "working" => Some((AgentStatus::Working, Some("osc7501_working"), false)),
        "blocked" => {
            let rule = match status.kind.as_deref() {
                Some("question") => "question_prompt",
                Some("auth") => "auth_prompt",
                _ => "permission_prompt",
            };
            Some((AgentStatus::Blocked, Some(rule), true))
        }
        "idle" | "done" | "error" => Some((AgentStatus::Idle, None, false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use spawn_core::agent_events::AgentEngine;

    use super::*;

    const RULE: &str = "────────────────────────────────────────────────────────";
    const BANNER: &str = "      ▄▀▀▄        Antigravity CLI 1.5.14\n\
                          \x20    ▀▀▀▀▀▀       someone@example.invalid (Pro)\n\
                          \x20   ▀▀▀▀▀▀▀▀      /tmp/ws/agent_a\n";

    #[test]
    fn process_classification_rejects_inline_shells_and_accepts_agents() {
        assert_eq!(
            classify_process("sh", &["/bin/sh".into(), "-c".into(), "echo agy".into()]),
            None
        );
        assert_eq!(
            classify_process("agy", &["/home/tiger/.local/bin/agy".into()]),
            Some(AgentEngine::Agy)
        );
        assert_eq!(
            classify_process("sh", &["/bin/sh".into(), "/tmp/scratch/bin/agy".into()]),
            Some(AgentEngine::Agy)
        );
        assert_eq!(
            classify_process(
                "node",
                &[
                    "node".into(),
                    "/usr/local/bin/claude".into(),
                    "--print".into()
                ]
            ),
            Some(AgentEngine::Claude)
        );
        assert_eq!(
            classify_process("codex", &["/usr/local/bin/codex".into()]),
            Some(AgentEngine::Codex)
        );
        assert_eq!(
            classify_process("opencode", &["opencode".into()]),
            Some(AgentEngine::OpenCode)
        );
        assert_eq!(
            classify_process("gemini", &["/usr/bin/gemini".into()]),
            Some(AgentEngine::Gemini)
        );
    }

    #[test]
    fn agy_witness_matches_every_manifest_state_and_strips_account_line() {
        let trust = "Accessing workspace:\n\n/tmp/ws/agent_a\n\n\
                     Do you trust the contents of this project?\n\n\
                     > Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n";
        let r = witness_screen(&AgentEngine::Agy, trust, None);
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("trust_prompt"));
        assert!(r.blocker_visible);

        let idle = format!("{BANNER}{RULE}\n>\n{RULE}\n? for shortcuts\n");
        let r = witness_screen(&AgentEngine::Agy, &idle, None);
        assert_eq!(r.status, AgentStatus::Idle);
        assert_eq!(r.rule, None);

        let tasks = format!(
            "{BANNER}{RULE}\n>\n{RULE}\n● Job A\n{RULE}\n? for shortcuts · 1 subagent(s) · 2 task(s) · /tasks\n"
        );
        let r = witness_screen(&AgentEngine::Agy, &tasks, None);
        assert_eq!(r.status, AgentStatus::Idle);
        assert_eq!(r.rule, None);

        let work = format!("{BANNER}⠋ Thinking about it\n{RULE}\n>\n{RULE}\nesc to cancel\n");
        let r = witness_screen(&AgentEngine::Agy, &work, None);
        assert_eq!(r.status, AgentStatus::Working);
        assert_eq!(r.rule.as_deref(), Some("esc_cancel_footer_working"));
        assert!(!r.blocker_visible);

        let perm = "Run command?\n  cargo test\n> 1. Yes\n  2. No\n\
                    ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\nesc to cancel\n";
        let r = witness_screen(&AgentEngine::Agy, perm, None);
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("permission_prompt"));
        assert!(r.blocker_visible);

        let ask =
            "Which one?\n> 1. A\n  2. B\n↑/↓ Navigate · enter Select · esc Skip\nesc to cancel\n";
        let r = witness_screen(&AgentEngine::Agy, ask, None);
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("question_prompt"));
        assert!(r.blocker_visible);
    }

    #[test]
    fn codex_and_generic_witnesses_classify_trust_osc_and_choice_footers() {
        let trust = "Since you are running Codex in /tmp/ws\nDo you trust the contents of this directory?\n1. Yes, continue\n2. No, quit\n";
        let r = witness_screen(&AgentEngine::Codex, trust, None);
        assert_eq!(r.agent.as_deref(), Some("codex"));
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("trust_prompt"));
        assert_eq!(r.manifest.as_deref(), Some(CODEX_MANIFEST));

        let osc_work = OscStatus {
            state: "working".into(),
            kind: None,
            progress: None,
            app: "opencode".into(),
            title: String::new(),
            msg: String::new(),
        };
        let r_opencode = witness_screen(&AgentEngine::OpenCode, "", Some(&osc_work));
        assert_eq!(r_opencode.agent.as_deref(), Some("opencode"));
        assert_eq!(r_opencode.status, AgentStatus::Working);
        assert_eq!(r_opencode.rule.as_deref(), Some("osc7501_working"));

        let choice_footer = "Allow command?\n1. Yes\n2. No\nSelect with numbers [1-2]. Then Enter to submit or Escape to cancel:\nEsc to cancel\n";
        let r_gemini = witness_screen(&AgentEngine::Gemini, choice_footer, None);
        assert_eq!(r_gemini.agent.as_deref(), Some("gemini"));
        assert_eq!(r_gemini.status, AgentStatus::Blocked);
        assert_eq!(r_gemini.rule.as_deref(), Some("permission_prompt"));
        assert!(r_gemini.blocker_visible);
    }

    #[test]
    fn osc7501_corroborates_status_while_forged_shell_never_reports_an_agent() {
        let osc = OscStatus {
            state: "working".into(),
            kind: None,
            progress: Some(50),
            app: "agy".into(),
            title: String::new(),
            msg: String::new(),
        };
        let reading = witness_screen(&AgentEngine::Agy, "", Some(&osc));
        assert_eq!(reading.status, AgentStatus::Working);
        assert_eq!(reading.rule.as_deref(), Some("osc7501_working"));

        let forged = observe(None, "Antigravity CLI\n>\nesc to cancel\n", Some(&osc));
        assert_eq!(forged, no_agent());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_tree_telemetry_samples_live_pid() {
        let tree = inspect_process_tree_in(&ProcSnapshot::capture(), std::process::id())
            .expect("live process tree");
        assert_eq!(tree.leader_pid, std::process::id());
        assert!(tree.process_count >= 1);
        assert_eq!(tree.child_count, tree.process_count.saturating_sub(1));
        assert!(!tree.processes.is_empty());
    }

    #[test]
    fn sanitized_command_redacts_secrets_and_caps_length() {
        let sanitized = format_sanitized_command(
            "curl",
            &[
                "curl".into(),
                "-H".into(),
                "Authorization: Bearer super-secret-tok-123".into(),
                "--api-key=sk-abc123".into(),
            ],
        );
        assert!(!sanitized.contains("super-secret-tok-123"), "{sanitized}");
        assert!(sanitized.len() <= MAX_CMDLINE_CHARS + 3, "{sanitized}");
    }
}
