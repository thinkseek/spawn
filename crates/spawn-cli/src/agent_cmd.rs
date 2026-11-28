//! `spawn agent`: zero-invasion global Herdr terminal discovery, snapshot, and
//! guard-checked TUI control CLI (Wave-AR AR-2).
//!
//! Uses closed Sum/Product ADTs over `spawn_core::tui` and
//! `spawn_host::herdr_global`: illegal flag combinations (`--if-guard` paired
//! with `--force`), invalid chords, malformed guards, and unobservable
//! `PlainTextOnly` screens all fail closed before mutating any terminal.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use spawn_core::tui::{
    DiscoveredTerminal, KeyChord, KeyChordSequence, ScreenGuard, SendPolicy, SnapshotFidelity,
    TerminalAddress, TerminalLiveState, TerminalOrigin, TuiAction, TuiActionOutcome, TuiSnapshot,
    WaitReason,
};
use spawn_host::herdr_global::{DiscoveryScope, HerdrGlobalClient, SocketScope};

macro_rules! agent_usage_body {
    ($first_prefix:literal) => {
        concat!(
            $first_prefix,
            "spawn agent ls [--all] [--socket <path>] [--json]\n",
            "       spawn agent snapshot <addr> [--socket <path>] [--json]\n",
            "       spawn agent send <addr> <text> [--if-guard <hash> | --force] [--socket <path>] [--json]\n",
            "       spawn agent keys <addr> <chord...> [--if-guard <hash>] [--socket <path>] [--json]\n",
            "       spawn agent interrupt <addr> [--timeout <duration>] [--socket <path>] [--json]\n",
            "       spawn agent wait <addr> [--timeout <duration>] [--after-turn <n>] [--output <text>] [--socket <path>] [--json]",
        )
    };
}
pub(crate) use agent_usage_body;

/// Usage lines printed by `spawn agent --help` and echoed verbatim in `main::MORE_USAGE`.
pub(crate) const USAGE: &str = agent_usage_body!("usage: ");

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SocketArg {
    Auto,
    Explicit(PathBuf),
}

impl SocketArg {
    fn resolve(&self) -> Result<SocketScope, String> {
        let explicit = match self {
            Self::Auto => None,
            Self::Explicit(path) => Some(path.as_path()),
        };
        SocketScope::resolve(explicit, &crate::config::herdr_discovery_env())
            .map_err(|err| err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentAction {
    Ls {
        all: bool,
    },
    Snapshot {
        addr: TerminalAddress,
    },
    Send {
        addr: TerminalAddress,
        text: String,
        policy: SendPolicy,
    },
    Keys {
        addr: TerminalAddress,
        keys: KeyChordSequence,
        expected_guard: Option<ScreenGuard>,
    },
    Interrupt {
        addr: TerminalAddress,
        timeout_ms: Option<u64>,
    },
    Wait {
        addr: TerminalAddress,
        timeout_ms: u64,
        after_turn: Option<u64>,
        output_contains: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedAgentCommand {
    socket: SocketArg,
    format: OutputFormat,
    action: AgentAction,
}

/// Entrypoint for `spawn agent ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let parsed = parse_args(rest)?;
    let scope = parsed.socket.resolve()?;
    match parsed.action {
        AgentAction::Ls { all } => {
            let terminals = HerdrGlobalClient::discover_in_scope(&scope, all, DiscoveryScope::All)
                .map_err(|err| err.to_string())?;
            match parsed.format {
                OutputFormat::Json => {
                    let payload = json!({
                        "agents": terminals,
                        "count": terminals.len(),
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&payload).map_err(|err| err.to_string())?
                    );
                }
                OutputFormat::Human => print!("{}", format_ls_human(&terminals)),
            }
            Ok(0)
        }
        AgentAction::Snapshot { addr } => {
            let client = HerdrGlobalClient::client_for_address(&scope, addr.as_str())
                .map_err(|err| err.to_string())?;
            let snap = client
                .snapshot(addr.as_str())
                .map_err(|err| err.to_string())?;
            match parsed.format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&snap).map_err(|err| err.to_string())?
                    );
                }
                OutputFormat::Human => print!("{}", format_snapshot_human(&snap)),
            }
            Ok(0)
        }
        AgentAction::Send { addr, text, policy } => {
            let action = TuiAction::SendText {
                text,
                policy,
                wait_timeout_ms: None,
            };
            execute_and_print(&scope, addr.as_str(), &action, parsed.format)
        }
        AgentAction::Keys {
            addr,
            keys,
            expected_guard,
        } => {
            let action = TuiAction::SendKeys {
                keys,
                expected_guard,
            };
            execute_and_print(&scope, addr.as_str(), &action, parsed.format)
        }
        AgentAction::Interrupt { addr, timeout_ms } => {
            let action = TuiAction::Interrupt { timeout_ms };
            execute_and_print(&scope, addr.as_str(), &action, parsed.format)
        }
        AgentAction::Wait {
            addr,
            timeout_ms,
            after_turn,
            output_contains,
        } => {
            let action = TuiAction::WaitSettled {
                after_turn,
                output_contains,
                timeout_ms,
            };
            execute_and_print(&scope, addr.as_str(), &action, parsed.format)
        }
    }
}

fn execute_and_print(
    scope: &SocketScope,
    addr: &str,
    action: &TuiAction,
    format: OutputFormat,
) -> Result<i32, String> {
    let client =
        HerdrGlobalClient::client_for_address(scope, addr).map_err(|err| err.to_string())?;
    let outcome = client
        .execute(addr, action)
        .map_err(|err| err.to_string())?;
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome).map_err(|err| err.to_string())?
            );
        }
        OutputFormat::Human => println!("{}", format_outcome_human(addr, &outcome)),
    }
    Ok(0)
}

fn parse_args(rest: &[String]) -> Result<ParsedAgentCommand, String> {
    let Some((verb, tail)) = rest.split_first() else {
        return Err(format!("agent subcommand required\n{USAGE}"));
    };
    match verb.as_str() {
        "ls" => parse_ls(tail),
        "snapshot" => parse_snapshot(tail),
        "send" => parse_send(tail),
        "keys" => parse_keys(tail),
        "interrupt" => parse_interrupt(tail),
        "wait" => parse_wait(tail),
        other => Err(format!("unknown agent subcommand {other:?}\n{USAGE}")),
    }
}

struct CommonState {
    socket: SocketArg,
    format: OutputFormat,
}

impl CommonState {
    fn new() -> Self {
        Self {
            socket: SocketArg::Auto,
            format: OutputFormat::Human,
        }
    }

    fn set_socket(&mut self, value: String) -> Result<(), String> {
        if !matches!(self.socket, SocketArg::Auto) {
            return Err("duplicate --socket flag".to_string());
        }
        if value.trim().is_empty() {
            return Err("--socket requires a non-empty path".to_string());
        }
        self.socket = SocketArg::Explicit(PathBuf::from(value));
        Ok(())
    }

    fn set_json(&mut self) {
        self.format = OutputFormat::Json;
    }
}

fn take_flag_value(
    flag: &str,
    inline: Option<&str>,
    iter: &mut std::iter::Peekable<std::slice::Iter<'_, String>>,
) -> Result<String, String> {
    if let Some(val) = inline {
        if val.is_empty() {
            return Err(format!("{flag} requires a value"));
        }
        return Ok(val.to_string());
    }
    let Some(next) = iter.next() else {
        return Err(format!("{flag} requires a value"));
    };
    Ok(next.clone())
}

fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((name, val)) => (name, Some(val)),
        None => (arg, None),
    }
}

fn parse_ls(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut all = false;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline) = split_flag(arg);
        match flag {
            "--all" if inline.is_none() => all = true,
            "--json" if inline.is_none() => common.set_json(),
            "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
            "--" if inline.is_none() => {
                if let Some(extra) = iter.next() {
                    return Err(format!("unexpected positional argument {extra:?} for ls"));
                }
                break;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag {arg:?} for `spawn agent ls`"));
            }
            other => {
                return Err(format!(
                    "unexpected positional argument {other:?} for `spawn agent ls`"
                ));
            }
        }
    }
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Ls { all },
    })
}

fn parse_snapshot(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut addr: Option<TerminalAddress> = None;
    let mut literal_only = false;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        if !literal_only && arg == "--" {
            literal_only = true;
            continue;
        }
        if !literal_only && arg.starts_with('-') {
            let (flag, inline) = split_flag(arg);
            match flag {
                "--json" if inline.is_none() => common.set_json(),
                "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
                _ => {
                    return Err(format!("unknown flag {arg:?} for `spawn agent snapshot`"));
                }
            }
            continue;
        }
        if addr.is_some() {
            return Err(format!(
                "unexpected extra argument {arg:?} for `spawn agent snapshot`"
            ));
        }
        addr = Some(TerminalAddress::parse(arg)?);
    }
    let addr = addr.ok_or_else(|| "usage: spawn agent snapshot <addr>".to_string())?;
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Snapshot { addr },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendGuardMode {
    Default,
    Force,
    IfGuard(ScreenGuard),
}

impl SendGuardMode {
    fn set_force(self) -> Result<Self, String> {
        match self {
            Self::Default | Self::Force => Ok(Self::Force),
            Self::IfGuard(_) => Err(
                "cannot combine --if-guard and --force: a guarded send cannot force past a blocked dialog".to_string(),
            ),
        }
    }

    fn set_if_guard(self, guard: ScreenGuard) -> Result<Self, String> {
        match self {
            Self::Default => Ok(Self::IfGuard(guard)),
            Self::IfGuard(_) => Err("duplicate --if-guard flag".to_string()),
            Self::Force => Err(
                "cannot combine --if-guard and --force: a guarded send cannot force past a blocked dialog".to_string(),
            ),
        }
    }

    fn into_policy(self) -> SendPolicy {
        match self {
            Self::Default => SendPolicy::WhenUnblocked,
            Self::Force => SendPolicy::ForceEvenIfBlocked,
            Self::IfGuard(expected_guard) => SendPolicy::OnlyIf {
                expected_guard,
                status: None,
            },
        }
    }
}

fn parse_send(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut mode = SendGuardMode::Default;
    let mut positionals: Vec<String> = Vec::new();
    let mut literal_only = false;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        if !literal_only && arg == "--" {
            literal_only = true;
            continue;
        }
        if !literal_only && arg.starts_with('-') {
            let (flag, inline) = split_flag(arg);
            match flag {
                "--json" if inline.is_none() => common.set_json(),
                "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
                "--force" if inline.is_none() => mode = mode.set_force()?,
                "--if-guard" => {
                    let raw = take_flag_value("--if-guard", inline, &mut iter)?;
                    let guard = ScreenGuard::parse(&raw)?;
                    mode = mode.set_if_guard(guard)?;
                }
                _ => return Err(format!("unknown flag {arg:?} for `spawn agent send`")),
            }
            continue;
        }
        positionals.push(arg.clone());
    }
    if positionals.len() != 2 {
        return Err(
            "usage: spawn agent send <addr> <text> [--if-guard <hash> | --force]".to_string(),
        );
    }
    let addr = TerminalAddress::parse(&positionals[0])?;
    let text = positionals.swap_remove(1);
    if text.is_empty() {
        return Err(
            "send text must not be empty (use `spawn agent keys <addr> enter` to press Enter)"
                .to_string(),
        );
    }
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Send {
            addr,
            text,
            policy: mode.into_policy(),
        },
    })
}

fn parse_keys(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut expected_guard: Option<ScreenGuard> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut literal_only = false;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        if !literal_only && arg == "--" {
            literal_only = true;
            continue;
        }
        if !literal_only && arg.starts_with('-') {
            let (flag, inline) = split_flag(arg);
            match flag {
                "--json" if inline.is_none() => common.set_json(),
                "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
                "--if-guard" => {
                    if expected_guard.is_some() {
                        return Err("duplicate --if-guard flag".to_string());
                    }
                    let raw = take_flag_value("--if-guard", inline, &mut iter)?;
                    expected_guard = Some(ScreenGuard::parse(&raw)?);
                }
                _ => return Err(format!("unknown flag {arg:?} for `spawn agent keys`")),
            }
            continue;
        }
        positionals.push(arg.clone());
    }
    if positionals.len() < 2 {
        return Err("usage: spawn agent keys <addr> <chord...> [--if-guard <hash>]".to_string());
    }
    let addr = TerminalAddress::parse(&positionals[0])?;
    let mut chords = Vec::with_capacity(positionals.len() - 1);
    for raw_chord in &positionals[1..] {
        chords.push(KeyChord::parse(raw_chord)?);
    }
    let keys = KeyChordSequence::new(chords)?;
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Keys {
            addr,
            keys,
            expected_guard,
        },
    })
}

fn parse_interrupt(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut addr: Option<TerminalAddress> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut literal_only = false;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        if !literal_only && arg == "--" {
            literal_only = true;
            continue;
        }
        if !literal_only && arg.starts_with('-') {
            let (flag, inline) = split_flag(arg);
            match flag {
                "--json" if inline.is_none() => common.set_json(),
                "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
                "--timeout" => {
                    if timeout_ms.is_some() {
                        return Err("duplicate --timeout flag".to_string());
                    }
                    let raw = take_flag_value("--timeout", inline, &mut iter)?;
                    timeout_ms = Some(parse_duration_ms(&raw)?);
                }
                _ => return Err(format!("unknown flag {arg:?} for `spawn agent interrupt`")),
            }
            continue;
        }
        if addr.is_some() {
            return Err(format!(
                "unexpected extra argument {arg:?} for `spawn agent interrupt`"
            ));
        }
        addr = Some(TerminalAddress::parse(arg)?);
    }
    let addr = addr.ok_or_else(|| "usage: spawn agent interrupt <addr>".to_string())?;
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Interrupt { addr, timeout_ms },
    })
}

fn parse_wait(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut addr: Option<TerminalAddress> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut after_turn: Option<u64> = None;
    let mut output_contains: Option<String> = None;
    let mut literal_only = false;
    let mut iter = tail.iter().peekable();
    while let Some(arg) = iter.next() {
        if !literal_only && arg == "--" {
            literal_only = true;
            continue;
        }
        if !literal_only && arg.starts_with('-') {
            let (flag, inline) = split_flag(arg);
            match flag {
                "--json" if inline.is_none() => common.set_json(),
                "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
                "--timeout" => {
                    if timeout_ms.is_some() {
                        return Err("duplicate --timeout flag".to_string());
                    }
                    let raw = take_flag_value("--timeout", inline, &mut iter)?;
                    timeout_ms = Some(parse_duration_ms(&raw)?);
                }
                "--after-turn" => {
                    if after_turn.is_some() {
                        return Err("duplicate --after-turn flag".to_string());
                    }
                    let raw = take_flag_value("--after-turn", inline, &mut iter)?;
                    let n = raw.parse::<u64>().map_err(|_| {
                        format!("--after-turn expects a non-negative integer, got {raw:?}")
                    })?;
                    after_turn = Some(n);
                }
                "--output" => {
                    if output_contains.is_some() {
                        return Err("duplicate --output flag".to_string());
                    }
                    let raw = take_flag_value("--output", inline, &mut iter)?;
                    output_contains = Some(raw);
                }
                _ => return Err(format!("unknown flag {arg:?} for `spawn agent wait`")),
            }
            continue;
        }
        if addr.is_some() {
            return Err(format!(
                "unexpected extra argument {arg:?} for `spawn agent wait`"
            ));
        }
        addr = Some(TerminalAddress::parse(arg)?);
    }
    let addr =
        addr.ok_or_else(|| "usage: spawn agent wait <addr> [--timeout <duration>]".to_string())?;
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Wait {
            addr,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS),
            after_turn,
            output_contains,
        },
    })
}

/// Parses compact duration strings (`500ms`, `5s`, `2m`, `1m30s`, `1h`, `1d`) into
/// positive milliseconds, rejecting `0`, malformed syntax, and overflow.
fn parse_duration_ms(raw: &str) -> Result<u64, String> {
    if raw.is_empty() {
        return Err("duration must not be empty".to_string());
    }
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut total_ms: u64 = 0;
    while i < bytes.len() {
        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == num_start || i >= bytes.len() {
            return Err(format!(
                "invalid duration {raw:?}: expected units like 500ms, 5s, 2m, 1h"
            ));
        }
        let qty: u64 = raw[num_start..i]
            .parse()
            .map_err(|_| format!("invalid duration number in {raw:?}"))?;
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let mult: u64 = match &raw[unit_start..i] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60 * 1_000,
            "h" => 60 * 60 * 1_000,
            "d" => 24 * 60 * 60 * 1_000,
            unit => {
                return Err(format!(
                    "invalid duration unit {unit:?} in {raw:?}: expected ms, s, m, h, or d"
                ));
            }
        };
        let segment = qty
            .checked_mul(mult)
            .ok_or_else(|| format!("duration {raw:?} overflows"))?;
        total_ms = total_ms
            .checked_add(segment)
            .ok_or_else(|| format!("duration {raw:?} overflows"))?;
    }
    if total_ms == 0 {
        return Err(format!("duration {raw:?} must be at least 1ms"));
    }
    if total_ms > MAX_WAIT_TIMEOUT_MS {
        return Err(format!(
            "duration {raw:?} exceeds maximum allowed {:?}",
            Duration::from_millis(MAX_WAIT_TIMEOUT_MS)
        ));
    }
    Ok(total_ms)
}

fn format_ls_human(terminals: &[DiscoveredTerminal]) -> String {
    if terminals.is_empty() {
        return "no terminals found\n".to_string();
    }
    let mut out = String::new();
    for term in terminals {
        let origin = match &term.origin {
            TerminalOrigin::SpawnManaged { owner_session, .. } => {
                format!("managed({owner_session})")
            }
            TerminalOrigin::External => "external".to_string(),
        };
        let state = match &term.state {
            TerminalLiveState::Live { pid: Some(pid) } => format!("live(pid={pid})"),
            TerminalLiveState::Live { pid: None } => "live".to_string(),
            TerminalLiveState::Exited { .. } => "exited".to_string(),
        };
        let agent = term.agent.as_deref().unwrap_or("-");
        let status = match term.agent_status {
            spawn_core::port::AgentStatus::Idle => "idle",
            spawn_core::port::AgentStatus::Working => "working",
            spawn_core::port::AgentStatus::Blocked => "blocked",
            spawn_core::port::AgentStatus::Unknown => "unknown",
        };
        let _ = writeln!(
            out,
            "{}\t{origin}\t{state}\t{agent}\t{status}\tturn={}\t{}",
            term.address, term.turn, term.title
        );
    }
    out
}

fn format_snapshot_human(snap: &TuiSnapshot) -> String {
    let mut out = String::new();
    let fidelity = match snap.fidelity() {
        SnapshotFidelity::StyledVt { alt_screen, .. } => {
            if *alt_screen {
                "styled_vt(alt)"
            } else {
                "styled_vt"
            }
        }
        SnapshotFidelity::PlainTextOnly { .. } => "plain_text_only",
    };
    let _ = writeln!(
        out,
        "address={} size={}x{} fidelity={} observable={} guard={} status={:?}",
        snap.address(),
        snap.cols(),
        snap.rows(),
        fidelity,
        snap.selection_observable(),
        snap.guard(),
        snap.agent_status(),
    );
    if !snap.choices().is_empty() {
        let _ = writeln!(out, "choices:");
        for choice in snap.choices() {
            let mark = if choice.selected { "*" } else { " " };
            let idx = choice
                .index
                .map_or_else(|| "-".to_string(), |n| n.to_string());
            let _ = writeln!(out, "  [{mark}] {idx}: {}", choice.label);
        }
    }
    for line in snap.plain_lines() {
        let _ = writeln!(out, "{line}");
    }
    out
}

fn format_outcome_human(addr: &str, outcome: &TuiActionOutcome) -> String {
    match outcome {
        TuiActionOutcome::TextSent {
            status,
            turn,
            started,
        } => format!(
            "sent {addr}: status={} turn={turn} started={}",
            status.as_deref().unwrap_or("-"),
            started.map_or_else(|| "-".to_string(), |v| v.to_string())
        ),
        TuiActionOutcome::KeysSent { status, turn } => format!(
            "keys {addr}: status={} turn={turn}",
            status.as_deref().unwrap_or("-")
        ),
        TuiActionOutcome::Interrupted(report) => format!(
            "interrupt {addr}: outcome={:?} key={}",
            report.outcome,
            report.key_sent.as_deref().unwrap_or("-")
        ),
        TuiActionOutcome::Waited(wait) => {
            let reason = match &wait.reason {
                WaitReason::Status => "status",
                WaitReason::Exit { .. } => "exit",
                WaitReason::Output => "output",
                WaitReason::Timeout => "timeout",
                WaitReason::NotStarted => "not_started",
                WaitReason::Children => "children",
                WaitReason::Message { .. } => "message",
                WaitReason::Other { raw, .. } => raw.as_str(),
            };
            format!(
                "wait {}: ended={} reason={reason} status={} turn={}",
                wait.terminal,
                wait.ended(),
                wait.status.as_deref().unwrap_or("-"),
                wait.turn
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vecs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn addr(raw: &str) -> TerminalAddress {
        TerminalAddress::parse(raw).expect("valid test terminal address")
    }

    #[test]
    fn parses_all_six_agent_subcommands_and_flags() {
        let ls = parse_args(&vecs(&["ls", "--all", "--socket", "/tmp/a.sock", "--json"]))
            .expect("parse ls");
        assert_eq!(ls.format, OutputFormat::Json);
        assert_eq!(ls.socket, SocketArg::Explicit(PathBuf::from("/tmp/a.sock")));
        assert_eq!(ls.action, AgentAction::Ls { all: true });

        let snap = parse_args(&vecs(&["snapshot", "n252/t3"])).expect("parse snapshot");
        assert_eq!(snap.format, OutputFormat::Human);
        assert_eq!(snap.socket, SocketArg::Auto);
        assert_eq!(
            snap.action,
            AgentAction::Snapshot {
                addr: addr("n252/t3")
            }
        );

        let g = ScreenGuard::from_raw(0xabcd);
        let send_guarded = parse_args(&vecs(&[
            "send",
            "t1",
            "hello world",
            "--if-guard",
            &g.to_string(),
            "--json",
        ]))
        .expect("parse guarded send");
        assert_eq!(
            send_guarded.action,
            AgentAction::Send {
                addr: addr("t1"),
                text: "hello world".to_string(),
                policy: SendPolicy::OnlyIf {
                    expected_guard: g,
                    status: None,
                },
            }
        );

        let send_forced =
            parse_args(&vecs(&["send", "t1", "--force", "--", "-dash-msg"])).expect("forced");
        assert_eq!(
            send_forced.action,
            AgentAction::Send {
                addr: addr("t1"),
                text: "-dash-msg".to_string(),
                policy: SendPolicy::ForceEvenIfBlocked,
            }
        );

        let keys = parse_args(&vecs(&[
            "keys",
            "t1",
            "down",
            "enter",
            "--if-guard",
            "0x42",
        ]))
        .expect("parse keys");
        assert!(matches!(
            keys.action,
            AgentAction::Keys {
                expected_guard: Some(guard),
                ..
            } if guard == ScreenGuard::from_raw(0x42)
        ));

        let interrupt =
            parse_args(&vecs(&["interrupt", "t1", "--timeout=1500ms"])).expect("interrupt");
        assert_eq!(
            interrupt.action,
            AgentAction::Interrupt {
                addr: addr("t1"),
                timeout_ms: Some(1500),
            }
        );

        let wait = parse_args(&vecs(&[
            "wait",
            "t1",
            "--timeout",
            "1m30s",
            "--after-turn",
            "7",
            "--output",
            "DONE",
            "--json",
        ]))
        .expect("wait");
        assert_eq!(
            wait.action,
            AgentAction::Wait {
                addr: addr("t1"),
                timeout_ms: 90_000,
                after_turn: Some(7),
                output_contains: Some("DONE".to_string()),
            }
        );
    }

    #[test]
    fn rejects_contradictory_send_flags_and_malformed_inputs_fail_closed() {
        for bad in [
            &[][..],
            &["bogus"],
            &["ls", "extra"],
            &["snapshot"],
            &["snapshot", "a/b/c"],
            &["snapshot", ".."],
            &["snapshot", "/t1"],
            &["snapshot", "t1/"],
            &["snapshot", "a//b"],
            &["send", "t1", "hi", "--submit"],
            &["send", "t1", "hi", "--if-guard", "0x1", "--force"],
            &["send", "t1", "hi", "--force", "--if-guard", "0x1"],
            &["send", "t1", ""],
            &["send", "t1", "hi", "--if-guard", "not-a-guard"],
            &["keys", "t1"],
            &["keys", "t1", "invalid_chord_name"],
            &["wait", "t1", "--timeout", "0s"],
            &["wait", "t1", "--timeout", "5x"],
            &["wait", "t1", "--after-turn", "-1"],
            &["snapshot", "t1", "--socket", ""],
        ] {
            assert!(parse_args(&vecs(bad)).is_err(), "must reject {bad:?}");
        }
    }
}
