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
use spawn_core::agent_events::{
    AgentHealth, DEFAULT_FUSED_EVENT_LIMIT, EventOrigin, FusedAgentEvent, FusedEventKind,
    FusedEventsReport, ObservabilitySource, UsageTotals,
};
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
            "       spawn agent events <addr> [--limit <n>] [--after-key <key>] [--follow] [--interval <dur>] [--socket <path>] [--json]\n",
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
const MAX_EVENTS_LIMIT: usize = 1_000;
const DEFAULT_FOLLOW_INTERVAL_MS: u64 = 1_000;
const MIN_FOLLOW_INTERVAL_MS: u64 = 100;
const MAX_FOLLOW_INTERVAL_MS: u64 = 60_000;
const MAX_AFTER_KEY_LEN: usize = 256;

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
    Events {
        addr: TerminalAddress,
        limit: usize,
        after_key: Option<String>,
        follow: bool,
        interval_ms: u64,
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
        AgentAction::Events {
            addr,
            limit,
            after_key,
            follow,
            interval_ms,
        } => run_events_command(
            &scope,
            parsed.format,
            &EventsRequest {
                addr: &addr,
                limit,
                after_key: after_key.as_deref(),
                follow,
                interval_ms,
            },
        ),
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
        "events" => parse_events(tail),
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

fn parse_events_limit(raw: &str) -> Result<usize, String> {
    let n = raw.trim().parse::<usize>().map_err(|_| {
        format!("invalid --limit {raw:?}: expected integer between 1 and {MAX_EVENTS_LIMIT}")
    })?;
    if !(1..=MAX_EVENTS_LIMIT).contains(&n) {
        return Err(format!(
            "--limit {n} out of range (expected 1..={MAX_EVENTS_LIMIT})"
        ));
    }
    Ok(n)
}

struct EventsRequest<'a> {
    addr: &'a TerminalAddress,
    limit: usize,
    after_key: Option<&'a str>,
    follow: bool,
    interval_ms: u64,
}

fn run_events_command(
    scope: &SocketScope,
    format: OutputFormat,
    req: &EventsRequest<'_>,
) -> Result<i32, String> {
    use std::io::Write as _;

    let client = HerdrGlobalClient::client_for_address(scope, req.addr.as_str())
        .map_err(|err| err.to_string())?;
    if !req.follow {
        let report = client
            .events_scoped_since(
                req.addr.as_str(),
                DiscoveryScope::All,
                req.limit,
                req.after_key,
            )
            .map_err(|err| err.to_string())?;
        match format {
            OutputFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|err| err.to_string())?
                );
            }
            OutputFormat::Human => print!("{}", format_events_human(&report)),
        }
        return Ok(0);
    }

    let mut cursor: Option<String> = req.after_key.map(str::to_string);
    let mut last_screen_keys: Option<Vec<String>> = None;
    let mut first_batch = true;
    loop {
        let raw_report = client
            .events_scoped_since(
                req.addr.as_str(),
                DiscoveryScope::All,
                req.limit,
                cursor.as_deref(),
            )
            .map_err(|err| err.to_string())?;
        let cursor_was_none = cursor.is_none();
        if let Some(latest) = &raw_report.latest_key {
            cursor = Some(latest.clone());
        }
        let report = filter_follow_report(
            raw_report,
            &mut last_screen_keys,
            first_batch,
            cursor_was_none,
        );
        let exited = matches!(report.summary.health, AgentHealth::Exited);
        emit_follow_batch(format, &report, first_batch, exited)?;
        let _ = std::io::stdout().flush();
        first_batch = false;
        if exited {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(req.interval_ms));
    }
}

fn filter_follow_report(
    mut report: FusedEventsReport,
    last_screen_keys: &mut Option<Vec<String>>,
    first_batch: bool,
    cursor_was_none: bool,
) -> FusedEventsReport {
    if report.latest_key.is_some() || !cursor_was_none {
        *last_screen_keys = None;
        return report;
    }
    let current_keys: Vec<String> = report
        .events
        .iter()
        .map(|ev| ev.canonical_key.clone())
        .collect();
    if !first_batch && let Some(prev) = last_screen_keys.as_ref() {
        report.events.retain(|ev| !prev.contains(&ev.canonical_key));
        report.count = report.events.len();
    }
    *last_screen_keys = Some(current_keys);
    report
}

fn emit_follow_batch(
    format: OutputFormat,
    report: &FusedEventsReport,
    first_batch: bool,
    exited: bool,
) -> Result<(), String> {
    match format {
        OutputFormat::Json => {
            if first_batch || !report.events.is_empty() || report.cursor_reset || exited {
                println!(
                    "{}",
                    serde_json::to_string(report).map_err(|err| err.to_string())?
                );
            }
        }
        OutputFormat::Human => {
            if first_batch {
                print!("{}", format_events_human(report));
            } else {
                if report.cursor_reset {
                    println!("cursor_reset: re-synced to latest window");
                }
                if !report.events.is_empty() {
                    let mut chunk = String::new();
                    append_event_lines(&mut chunk, &report.events);
                    print!("{chunk}");
                }
                if exited {
                    println!("health: exited");
                }
            }
        }
    }
    Ok(())
}

fn parse_events(tail: &[String]) -> Result<ParsedAgentCommand, String> {
    let mut common = CommonState::new();
    let mut addr: Option<TerminalAddress> = None;
    let mut limit = DEFAULT_FUSED_EVENT_LIMIT;
    let mut after_key: Option<String> = None;
    let mut follow = false;
    let mut interval_ms = DEFAULT_FOLLOW_INTERVAL_MS;
    let mut interval_set = false;
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
                "--follow" if inline.is_none() => follow = true,
                "--socket" => common.set_socket(take_flag_value("--socket", inline, &mut iter)?)?,
                "--limit" => {
                    limit = parse_events_limit(&take_flag_value("--limit", inline, &mut iter)?)?;
                }
                "--after-key" => {
                    after_key = Some(parse_after_key(&take_flag_value(
                        "--after-key",
                        inline,
                        &mut iter,
                    )?)?);
                }
                "--interval" => {
                    interval_ms =
                        parse_interval_ms(&take_flag_value("--interval", inline, &mut iter)?)?;
                    interval_set = true;
                }
                _ => {
                    return Err(format!("unknown flag {arg:?} for `spawn agent events`"));
                }
            }
            continue;
        }
        if addr.is_some() {
            return Err(format!(
                "unexpected extra argument {arg:?} for `spawn agent events`"
            ));
        }
        addr = Some(TerminalAddress::parse(arg)?);
    }
    if interval_set && !follow {
        return Err("`--interval` requires `--follow` for `spawn agent events`".to_string());
    }
    let addr = addr.ok_or_else(|| "usage: spawn agent events <addr>".to_string())?;
    Ok(ParsedAgentCommand {
        socket: common.socket,
        format: common.format,
        action: AgentAction::Events {
            addr,
            limit,
            after_key,
            follow,
            interval_ms,
        },
    })
}

fn parse_after_key(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("`--after-key` must not be empty".to_string());
    }
    if raw.len() > MAX_AFTER_KEY_LEN {
        return Err(format!(
            "`--after-key` exceeds maximum length of {MAX_AFTER_KEY_LEN} bytes"
        ));
    }
    if raw.chars().any(char::is_control) {
        return Err("`--after-key` must not contain control characters".to_string());
    }
    Ok(raw.to_string())
}

fn parse_interval_ms(raw: &str) -> Result<u64, String> {
    let ms = parse_duration_ms(raw)?;
    if !(MIN_FOLLOW_INTERVAL_MS..=MAX_FOLLOW_INTERVAL_MS).contains(&ms) {
        return Err(format!(
            "invalid `--interval` {raw:?}: expected between {MIN_FOLLOW_INTERVAL_MS}ms and {MAX_FOLLOW_INTERVAL_MS}ms"
        ));
    }
    Ok(ms)
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
        let session = term.session.as_ref().map_or_else(
            || "-".to_string(),
            |sess| {
                sess.native_session_id
                    .as_deref()
                    .unwrap_or(&sess.path)
                    .to_string()
            },
        );
        let proc_col = term.telemetry.as_ref().map_or_else(
            || "proc=-\tdisc=-".to_string(),
            |tel| {
                let cmd = tel.active_command.as_deref().unwrap_or("-");
                let disc = tel.discovery_status.short_reason();
                format!(
                    "proc={}({}c,{}KiB,cmd={cmd})\tdisc={disc}",
                    tel.process_count, tel.child_count, tel.total_rss_kib
                )
            },
        );
        let _ = writeln!(
            out,
            "{}\t{origin}\t{state}\t{agent}\t{status}\tturn={}\tsession={session}\t{proc_col}\t{}",
            term.address, term.turn, term.title
        );
    }
    out
}

fn format_health(health: &AgentHealth) -> String {
    match health {
        AgentHealth::Healthy => "healthy".to_string(),
        AgentHealth::Busy => "busy".to_string(),
        AgentHealth::Blocked { rule } => format!("blocked({rule})"),
        AgentHealth::Stalled { silent_ms } => format!("stalled({}s)", silent_ms / 1_000),
        AgentHealth::Erroring { error_count } => format!("erroring(errors={error_count})"),
        AgentHealth::Degraded { reason } => format!("degraded({reason})"),
        AgentHealth::Exited => "exited".to_string(),
    }
}

fn format_sources(sources: &std::collections::BTreeSet<ObservabilitySource>) -> String {
    if sources.is_empty() {
        return "-".to_string();
    }
    sources
        .iter()
        .map(|src| match src {
            ObservabilitySource::HerdrPty => "pty",
            ObservabilitySource::NativeSession => "session",
            ObservabilitySource::SpawnControl => "control",
            ObservabilitySource::ProcTree => "proc",
            ObservabilitySource::Osc7501 => "osc",
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn format_events_human(report: &FusedEventsReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "address={} engine={} fidelity={:?} count={} truncated={} latest_key={} cursor_reset={}",
        report.address,
        report.engine.as_deref().unwrap_or("-"),
        report.fidelity,
        report.count,
        report.truncated,
        report.latest_key.as_deref().unwrap_or("-"),
        report.cursor_reset,
    );
    append_observability_header(&mut out, report);
    if let Some(sess) = &report.session {
        let _ = writeln!(
            out,
            "session: path={} format={:?} method={:?} confidence={:?} id={}",
            sess.path,
            sess.format,
            sess.method,
            sess.confidence,
            sess.native_session_id.as_deref().unwrap_or("-"),
        );
    }
    append_event_lines(&mut out, &report.events);
    out
}

fn append_observability_header(out: &mut String, report: &FusedEventsReport) {
    let sum = &report.summary;
    let _ = writeln!(
        out,
        "health={} discovery={} sources={} turns={} tools={} tool_errs={} errs={} blocked={}",
        format_health(&sum.health),
        sum.discovery_status.short_reason(),
        format_sources(&sum.sources_observed),
        sum.turn_count,
        sum.tool_call_count,
        sum.tool_error_count,
        sum.error_count,
        sum.blocked_count,
    );
    if !sum.in_flight_tools.is_empty() {
        let joined = sum
            .in_flight_tools
            .iter()
            .map(|t| format!("{}({})", t.tool_name, t.summary))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "in_flight: {joined}");
    }
    if sum.usage_totals != UsageTotals::default() {
        let u = &sum.usage_totals;
        let _ = writeln!(
            out,
            "usage: model={} in={} out={} cache_read={} cache_write={}",
            u.last_model.as_deref().unwrap_or("-"),
            u.input_tokens,
            u.output_tokens,
            u.cache_read_tokens,
            u.cache_write_tokens,
        );
    }
    if let Some(tree) = &report.process_tree {
        let _ = writeln!(
            out,
            "proc_tree: leader={} procs={} children={} rss_kib={} leaf={}",
            tree.leader_pid,
            tree.process_count,
            tree.child_count,
            tree.total_rss_kib,
            tree.active_leaf_command.as_deref().unwrap_or("-"),
        );
    }
}

fn append_event_lines(out: &mut String, events: &[FusedAgentEvent]) {
    for ev in events {
        let ts = ev
            .timestamp_ms
            .map_or_else(|| "-".to_string(), |v| v.to_string());
        let origins = ev
            .origins
            .iter()
            .map(|origin| match origin {
                EventOrigin::HerdrPty => "pty",
                EventOrigin::NativeSession => "session",
                EventOrigin::SpawnControl => "control",
            })
            .collect::<Vec<_>>()
            .join("+");
        let desc = describe_fused_event_kind(&ev.kind);
        let _ = writeln!(out, "{ts}\t[{origins}]\t{desc}");
    }
}

fn describe_fused_event_kind(kind: &FusedEventKind) -> String {
    match kind {
        FusedEventKind::SessionMeta {
            engine, cwd, model, ..
        } => format!(
            "session_meta engine={} model={} cwd={}",
            engine.as_deref().unwrap_or("-"),
            model.as_deref().unwrap_or("-"),
            cwd.as_deref().unwrap_or("-")
        ),
        FusedEventKind::TurnStarted { turn_id } => {
            format!("turn_started id={}", turn_id.as_deref().unwrap_or("-"))
        }
        FusedEventKind::TurnCompleted {
            turn_id,
            duration_ms,
            error,
        } => format!(
            "turn_completed id={} duration_ms={} error={}",
            turn_id.as_deref().unwrap_or("-"),
            duration_ms.map_or_else(|| "-".to_string(), |d| d.to_string()),
            error.as_deref().unwrap_or("-")
        ),
        FusedEventKind::UserInput { summary } => format!("user: {summary}"),
        FusedEventKind::Thinking { summary, .. } => format!("thinking: {summary}"),
        FusedEventKind::AssistantMessage { summary } => format!("assistant: {summary}"),
        FusedEventKind::ToolStarted {
            tool_name, summary, ..
        } => format!("tool_start({tool_name}): {summary}"),
        FusedEventKind::ToolCompleted {
            tool_name,
            ok,
            summary,
            ..
        } => format!("tool_done({tool_name}, ok={ok}): {summary}"),
        FusedEventKind::UsageRecorded {
            model,
            input_tokens,
            output_tokens,
            ..
        } => format!(
            "usage model={} in={input_tokens} out={output_tokens}",
            model.as_deref().unwrap_or("-")
        ),
        FusedEventKind::PromptBlocked {
            rule,
            choices_count,
        } => format!("prompt_blocked rule={rule} choices={choices_count}"),
        FusedEventKind::ProcessState { status, rule } => format!(
            "process_state status={status} rule={}",
            rule.as_deref().unwrap_or("-")
        ),
        FusedEventKind::ErrorObserved { message } => format!("error: {message}"),
    }
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

        let snapshot = parse_args(&vecs(&["snapshot", "t1", "--json"])).expect("parse snapshot");
        assert_eq!(snapshot.action, AgentAction::Snapshot { addr: addr("t1") });

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
    fn parses_agent_events_subcommand_and_limit() {
        let events =
            parse_args(&vecs(&["events", "t1", "--limit", "25", "--json"])).expect("parse events");
        assert_eq!(events.format, OutputFormat::Json);
        assert_eq!(
            events.action,
            AgentAction::Events {
                addr: addr("t1"),
                limit: 25,
                after_key: None,
                follow: false,
                interval_ms: DEFAULT_FOLLOW_INTERVAL_MS,
            }
        );

        let events_default = parse_args(&vecs(&["events", "t1"])).expect("events default");
        assert_eq!(
            events_default.action,
            AgentAction::Events {
                addr: addr("t1"),
                limit: DEFAULT_FUSED_EVENT_LIMIT,
                after_key: None,
                follow: false,
                interval_ms: DEFAULT_FOLLOW_INTERVAL_MS,
            }
        );

        let followed = parse_args(&vecs(&[
            "events",
            "t1",
            "--after-key",
            "tool:call_1:started",
            "--follow",
            "--interval",
            "500ms",
        ]))
        .expect("parse events follow");
        assert_eq!(
            followed.action,
            AgentAction::Events {
                addr: addr("t1"),
                limit: DEFAULT_FUSED_EVENT_LIMIT,
                after_key: Some("tool:call_1:started".to_string()),
                follow: true,
                interval_ms: 500,
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
            &["events"],
            &["events", "t1", "--limit", "0"],
            &["events", "t1", "--limit", "1001"],
            &["events", "t1", "--limit", "abc"],
            &["events", "t1", "--after-key", ""],
            &["events", "t1", "--after-key", "bad\nkey"],
            &["events", "t1", "--interval", "50ms", "--follow"],
            &["events", "t1", "--interval", "5m", "--follow"],
            &["events", "t1", "--interval", "500ms"],
            &["events", "t1", "--follow=1"],
            &["events", "t1", "extra"],
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

    fn sample_human_events_report() -> FusedEventsReport {
        use spawn_core::agent_events::{
            AgentObservabilitySummary, DiscoveryConfidence, EventFidelity, FusedAgentEvent,
            InFlightTool, ProcessNode, ProcessTreeTelemetry, SessionAttachment,
            SessionDiscoveryMethod, SessionDiscoveryStatus, SessionFileFormat,
        };
        use std::collections::BTreeSet;

        FusedEventsReport {
            address: "t1".to_string(),
            engine: Some("claude".to_string()),
            session: Some(SessionAttachment {
                path: "/tmp/s1.jsonl".to_string(),
                format: SessionFileFormat::ClaudeJsonl,
                method: SessionDiscoveryMethod::ProcessOpenFd,
                confidence: DiscoveryConfidence::ExactFd,
                native_session_id: Some("sid-1".to_string()),
                cwd: Some("/repo".to_string()),
            }),
            fidelity: EventFidelity::SessionEnriched,
            summary: AgentObservabilitySummary {
                health: AgentHealth::Busy,
                discovery_status: SessionDiscoveryStatus::Attached {
                    method: SessionDiscoveryMethod::ProcessOpenFd,
                    confidence: DiscoveryConfidence::ExactFd,
                },
                sources_observed: BTreeSet::from([
                    ObservabilitySource::NativeSession,
                    ObservabilitySource::HerdrPty,
                    ObservabilitySource::ProcTree,
                ]),
                turn_count: 1,
                tool_call_count: 1,
                tool_error_count: 0,
                error_count: 0,
                blocked_count: 0,
                in_flight_tools: vec![InFlightTool {
                    tool_call_id: Some("t1".to_string()),
                    tool_name: "Bash".to_string(),
                    summary: "cargo test".to_string(),
                    started_at_ms: Some(1_795_860_000_000),
                }],
                usage_totals: UsageTotals {
                    last_model: Some("claude-sonnet".to_string()),
                    input_tokens: 1200,
                    output_tokens: 400,
                    cache_read_tokens: 50,
                    cache_write_tokens: 10,
                },
                first_event_at_ms: Some(1_795_860_000_000),
                last_event_at_ms: Some(1_795_860_000_000),
                last_tool_name: Some("Bash".to_string()),
                last_error: None,
            },
            process_tree: Some(ProcessTreeTelemetry {
                leader_pid: 4242,
                process_count: 2,
                child_count: 1,
                total_rss_kib: 12_288,
                active_leaf_command: Some("cargo test".to_string()),
                processes: vec![ProcessNode {
                    pid: 4242,
                    ppid: 1,
                    comm: "claude".to_string(),
                    state: "S".to_string(),
                    rss_kib: 12_288,
                    command: "claude".to_string(),
                }],
            }),
            latest_key: Some("tool:t1:started".to_string()),
            cursor_reset: false,
            count: 1,
            truncated: false,
            events: vec![FusedAgentEvent {
                canonical_key: "tool:t1:started".to_string(),
                timestamp_ms: Some(1_795_860_000_000),
                fidelity: EventFidelity::SessionEnriched,
                origins: BTreeSet::from([EventOrigin::NativeSession, EventOrigin::HerdrPty]),
                kind: FusedEventKind::ToolStarted {
                    tool_call_id: Some("t1".to_string()),
                    tool_name: "Bash".to_string(),
                    summary: "cargo test".to_string(),
                },
                session_id: Some("sid-1".to_string()),
                terminal_address: Some("t1".to_string()),
            }],
        }
    }

    #[test]
    fn format_events_human_displays_session_fidelity_and_event_lines() {
        let report = sample_human_events_report();
        let rendered = format_events_human(&report);
        assert!(
            rendered.contains("address=t1 engine=claude fidelity=SessionEnriched"),
            "{rendered}"
        );
        assert!(rendered.contains("health=busy"), "{rendered}");
        assert!(
            rendered.contains("in_flight: Bash(cargo test)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("usage: model=claude-sonnet in=1200 out=400"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "proc_tree: leader=4242 procs=2 children=1 rss_kib=12288 leaf=cargo test"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("session: path=/tmp/s1.jsonl"),
            "{rendered}"
        );
        assert!(
            rendered.contains("[pty+session]\ttool_start(Bash): cargo test"),
            "{rendered}"
        );
    }

    #[test]
    fn follow_filter_suppresses_repeated_screen_events_and_switches_to_session_cursor() {
        let screen_only = FusedEventsReport {
            latest_key: None,
            ..sample_human_events_report()
        };
        let mut screen_keys = None;
        let first = filter_follow_report(screen_only.clone(), &mut screen_keys, true, true);
        assert_eq!(first.events.len(), 1);

        let repeat = filter_follow_report(screen_only, &mut screen_keys, false, true);
        assert!(repeat.events.is_empty());

        let with_session = sample_human_events_report();
        let resumed = filter_follow_report(with_session, &mut screen_keys, false, true);
        assert_eq!(resumed.events.len(), 1);
        assert!(screen_keys.is_none());
    }
}
