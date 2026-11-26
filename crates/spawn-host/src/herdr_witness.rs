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

/// Interpreters whose first non-flag script argument names the executed agent
/// (when not invoked via `-c` / `-e` inline shell snippets).
const INTERPRETERS: [&str; 11] = [
    "sh", "bash", "dash", "zsh", "env", "node", "nodejs", "bun", "deno", "python", "python3",
];

/// Supported coding-agent engines identifiable in the host process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedAgent {
    /// Antigravity CLI (`agy`) process.
    Agy,
    /// Anthropic Claude Code (`claude`) process.
    Claude,
}

impl DetectedAgent {
    /// Canonical Spawn agent identifier.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::Claude => "claude",
        }
    }

    /// Host witness manifest version string.
    const fn manifest(self) -> &'static str {
        match self {
            Self::Agy => AGY_MANIFEST,
            Self::Claude => CLAUDE_MANIFEST,
        }
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

/// Identifies whether `comm` / `argv` corresponds to a real `agy` or `claude` process.
///
/// Inline shell snippets (`sh -c "agy ..."`) are explicitly ignored so a plain shell
/// running commands named after an agent never counts as an agent (`N2`).
fn classify_process(comm: &str, argv: &[String]) -> Option<DetectedAgent> {
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

fn match_binary_name(name: &str) -> Option<DetectedAgent> {
    match name {
        "agy" => Some(DetectedAgent::Agy),
        "claude" => Some(DetectedAgent::Claude),
        _ => None,
    }
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

/// Walks the live `/proc` process tree rooted at `leader_pid` and detects
/// the first live `agy` or `claude` descendant (or the leader itself).
fn detect_agent_in_tree(leader_pid: u32) -> Option<DetectedAgent> {
    if leader_pid <= 1 {
        return None;
    }
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut leader_alive = false;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return None;
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
            if pid == leader_pid {
                leader_alive = true;
            }
            children.entry(ppid).or_default().push(pid);
        }
    }
    if !leader_alive {
        return None;
    }
    let mut queue = VecDeque::from([leader_pid]);
    let mut seen = HashSet::from([leader_pid]);
    while let Some(pid) = queue.pop_front() {
        if let Some(agent) = inspect_pid(pid) {
            return Some(agent);
        }
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                if seen.insert(kid) {
                    queue.push_back(kid);
                }
            }
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

fn inspect_pid(pid: u32) -> Option<DetectedAgent> {
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
    classify_process(&comm, &argv)
}

/// Combines `/proc` process verification with screen and optional OSC 7501 observation.
pub(crate) fn observe(
    leader_pid: Option<u32>,
    raw_screen: &str,
    osc_status: Option<&OscStatus>,
) -> AgentReading {
    let Some(agent) = leader_pid.and_then(detect_agent_in_tree) else {
        return no_agent();
    };
    witness_screen(agent, raw_screen, osc_status)
}

/// Evaluates `raw_screen` and optional `osc_status` for a verified `agent`.
fn witness_screen(
    agent: DetectedAgent,
    raw_screen: &str,
    osc_status: Option<&OscStatus>,
) -> AgentReading {
    let clean = drop_agy_account_lines(raw_screen);
    let (status, rule, blocker_visible) = match agent {
        DetectedAgent::Agy => witness_agy(&clean, osc_status),
        DetectedAgent::Claude => witness_claude(&clean, osc_status),
    };
    AgentReading {
        agent: Some(agent.as_str().to_string()),
        status,
        rule: rule.map(str::to_string),
        blocker_visible,
        session: None,
        seq: 0,
        manifest: Some(agent.manifest().to_string()),
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
            Some(DetectedAgent::Agy)
        );
        assert_eq!(
            classify_process("sh", &["/bin/sh".into(), "/tmp/scratch/bin/agy".into()]),
            Some(DetectedAgent::Agy)
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
            Some(DetectedAgent::Claude)
        );
    }

    #[test]
    fn agy_witness_matches_every_manifest_state_and_strips_account_line() {
        let trust = "Accessing workspace:\n\n/tmp/ws/agent_a\n\n\
                     Do you trust the contents of this project?\n\n\
                     > Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n";
        let r = witness_screen(DetectedAgent::Agy, trust, None);
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("trust_prompt"));
        assert!(r.blocker_visible);

        let idle = format!("{BANNER}{RULE}\n>\n{RULE}\n? for shortcuts\n");
        let r = witness_screen(DetectedAgent::Agy, &idle, None);
        assert_eq!(r.status, AgentStatus::Idle);
        assert_eq!(r.rule, None);

        let tasks = format!(
            "{BANNER}{RULE}\n>\n{RULE}\n● Job A\n{RULE}\n? for shortcuts · 1 subagent(s) · 2 task(s) · /tasks\n"
        );
        let r = witness_screen(DetectedAgent::Agy, &tasks, None);
        assert_eq!(r.status, AgentStatus::Idle);
        assert_eq!(r.rule, None);

        let work = format!("{BANNER}⠋ Thinking about it\n{RULE}\n>\n{RULE}\nesc to cancel\n");
        let r = witness_screen(DetectedAgent::Agy, &work, None);
        assert_eq!(r.status, AgentStatus::Working);
        assert_eq!(r.rule.as_deref(), Some("esc_cancel_footer_working"));
        assert!(!r.blocker_visible);

        let perm = "Run command?\n  cargo test\n> 1. Yes\n  2. No\n\
                    ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\nesc to cancel\n";
        let r = witness_screen(DetectedAgent::Agy, perm, None);
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("permission_prompt"));
        assert!(r.blocker_visible);

        let ask =
            "Which one?\n> 1. A\n  2. B\n↑/↓ Navigate · enter Select · esc Skip\nesc to cancel\n";
        let r = witness_screen(DetectedAgent::Agy, ask, None);
        assert_eq!(r.status, AgentStatus::Blocked);
        assert_eq!(r.rule.as_deref(), Some("question_prompt"));
        assert!(r.blocker_visible);
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
        let reading = witness_screen(DetectedAgent::Agy, "", Some(&osc));
        assert_eq!(reading.status, AgentStatus::Working);
        assert_eq!(reading.rule.as_deref(), Some("osc7501_working"));

        let forged = observe(None, "Antigravity CLI\n>\nesc to cancel\n", Some(&osc));
        assert_eq!(forged, no_agent());
    }
}
