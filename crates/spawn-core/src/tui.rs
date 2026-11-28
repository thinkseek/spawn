//! Pure Agent Runtime TUI & zero-invasion Herdr control domain types.
//!
//! Models unfiltered global terminal discovery, full VT/xterm key chords,
//! structured TUI snapshots with deterministic FNV-1a CAS hashes, and unified
//! message/interrupt/wait actions without performing any IO or reading clocks.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::command::NamedKey;
use crate::port::AgentStatus;

/// Label key written by Spawn's managed `HerdrCoreHost` on owned terminals.
pub const SPAWN_OWNER_LABEL: &str = "spawn_owner";

/// Label key holding the logical `TerminalName` on Spawn-owned terminals.
pub const SPAWN_TERMINAL_LABEL: &str = "spawn_terminal";

/// Label key holding the `WorkspaceId` string on Spawn-owned terminals.
pub const SPAWN_WORKSPACE_LABEL: &str = "spawn_workspace";

/// Modifier bits carried by a [`KeyChord`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KeyModifiers {
    /// Control modifier (`ctrl+...`).
    pub ctrl: bool,
    /// Alt / Meta / Option modifier (`alt+...`).
    pub alt: bool,
    /// Shift modifier (`shift+...`).
    pub shift: bool,
}

impl KeyModifiers {
    /// Empty modifier set.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
        }
    }
}

/// Primary key within a [`KeyChord`], aligned with `herdr-core::keys::encode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// Escape key (`esc`).
    Escape,
    /// Enter / Return key (`enter`).
    Enter,
    /// Tab key (`tab`).
    Tab,
    /// Backspace key (`backspace`).
    Backspace,
    /// Space bar (`space`).
    Space,
    /// Delete key (`delete`).
    Delete,
    /// Insert key (`insert`).
    Insert,
    /// Home key (`home`).
    Home,
    /// End key (`end`).
    End,
    /// PageUp key (`pageup`).
    PageUp,
    /// PageDown key (`pagedown`).
    PageDown,
    /// Up arrow (`up`).
    Up,
    /// Down arrow (`down`).
    Down,
    /// Left arrow (`left`).
    Left,
    /// Right arrow (`right`).
    Right,
    /// Function key (`f1` through `f12`).
    Function(u8),
    /// Printable ASCII key supported by Herdr's VT encoder (`a`..`z`, `0`..`9`,
    /// or `` ` \ [ ] , = - . ' ; / ``).
    Char(char),
}

impl KeyCode {
    fn from_token(token: &str) -> Result<Self, String> {
        let lower = token.to_ascii_lowercase();
        let single_char = {
            let mut chars = lower.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Some(ch),
                _ => None,
            }
        };
        if let Some(ch) = single_char
            && is_supported_key_char(ch)
        {
            return Ok(Self::Char(ch));
        }
        match lower.as_str() {
            "esc" | "escape" => Ok(Self::Escape),
            "enter" | "return" => Ok(Self::Enter),
            "tab" => Ok(Self::Tab),
            "backspace" => Ok(Self::Backspace),
            "space" => Ok(Self::Space),
            "delete" | "del" => Ok(Self::Delete),
            "insert" => Ok(Self::Insert),
            "home" => Ok(Self::Home),
            "end" => Ok(Self::End),
            "pageup" | "pgup" => Ok(Self::PageUp),
            "pagedown" | "pgdn" => Ok(Self::PageDown),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "backslash" => Ok(Self::Char('\\')),
            d if d.starts_with("digit") && d.len() == 6 => {
                let ch = d.chars().nth(5).unwrap_or_default();
                if ('1'..='9').contains(&ch) {
                    Ok(Self::Char(ch))
                } else {
                    Err(format!("unsupported digit key {token:?}"))
                }
            }
            f if f.starts_with('f') && f.len() <= 3 => {
                let num: u8 = f[1..]
                    .parse()
                    .map_err(|_| format!("unsupported function key {token:?}"))?;
                if (1..=12).contains(&num) {
                    Ok(Self::Function(num))
                } else {
                    Err(format!("function key {token:?} must be within f1..=f12"))
                }
            }
            _ => Err(format!("unsupported VT key {token:?}")),
        }
    }

    fn push_herdr_name(self, out: &mut String) {
        match self {
            Self::Escape => out.push_str("esc"),
            Self::Enter => out.push_str("enter"),
            Self::Tab => out.push_str("tab"),
            Self::Backspace => out.push_str("backspace"),
            Self::Space => out.push_str("space"),
            Self::Delete => out.push_str("delete"),
            Self::Insert => out.push_str("insert"),
            Self::Home => out.push_str("home"),
            Self::End => out.push_str("end"),
            Self::PageUp => out.push_str("pageup"),
            Self::PageDown => out.push_str("pagedown"),
            Self::Up => out.push_str("up"),
            Self::Down => out.push_str("down"),
            Self::Left => out.push_str("left"),
            Self::Right => out.push_str("right"),
            Self::Function(n) => {
                out.push('f');
                out.push_str(&n.to_string());
            }
            Self::Char(ch) => out.push(ch),
        }
    }
}

fn is_supported_key_char(ch: char) -> bool {
    ch.is_ascii_lowercase()
        || ch.is_ascii_digit()
        || matches!(
            ch,
            '`' | '\\' | '[' | ']' | ',' | '=' | '-' | '.' | '\'' | ';' | '/'
        )
}

/// Canonical VT key chord sent to `herdr-core` `terminal.keys`.
///
/// Serializes and deserializes as its compact specification string
/// (for example `"ctrl+c"`, `"ctrl+shift+tab"`, `"down"`, `"enter"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    /// Active key modifiers.
    pub modifiers: KeyModifiers,
    /// Primary key code.
    pub code: KeyCode,
}

impl KeyChord {
    /// Builds a [`KeyChord`] without modifiers.
    #[must_use]
    pub const fn plain(code: KeyCode) -> Self {
        Self {
            modifiers: KeyModifiers::none(),
            code,
        }
    }

    /// Fail-closed parser for VT key specs (`"ctrl+c"`, `"shift+tab"`, `"f5"`, `"down"`).
    ///
    /// Rejects empty strings, any ASCII/Unicode whitespace, empty `+` segments,
    /// duplicate modifiers, and unsupported keys or modifiers.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() || raw.chars().any(char::is_whitespace) {
            return Err(format!(
                "invalid key chord {raw:?}: empty or contains whitespace"
            ));
        }
        let parts: Vec<&str> = raw.split('+').collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(format!("invalid key chord {raw:?}: empty '+' segment"));
        }
        let (&key_token, modifier_tokens) = parts
            .split_last()
            .ok_or_else(|| format!("invalid key chord {raw:?}"))?;
        let mut modifiers = KeyModifiers::none();
        for token in modifier_tokens {
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => {
                    if modifiers.ctrl {
                        return Err(format!("duplicate ctrl modifier in {raw:?}"));
                    }
                    modifiers.ctrl = true;
                }
                "alt" | "meta" | "opt" => {
                    if modifiers.alt {
                        return Err(format!("duplicate alt modifier in {raw:?}"));
                    }
                    modifiers.alt = true;
                }
                "shift" => {
                    if modifiers.shift {
                        return Err(format!("duplicate shift modifier in {raw:?}"));
                    }
                    modifiers.shift = true;
                }
                other => return Err(format!("unknown modifier {other:?} in {raw:?}")),
            }
        }
        let code = KeyCode::from_token(key_token)?;
        Ok(Self { modifiers, code })
    }

    /// Canonical specification string accepted by `herdr-core::keys::encode`.
    #[must_use]
    pub fn to_herdr_spec(&self) -> String {
        let mut out = String::new();
        if self.modifiers.ctrl {
            out.push_str("ctrl+");
        }
        if self.modifiers.alt {
            out.push_str("alt+");
        }
        if self.modifiers.shift {
            out.push_str("shift+");
        }
        self.code.push_herdr_name(&mut out);
        out
    }
}

impl From<NamedKey> for KeyChord {
    fn from(key: NamedKey) -> Self {
        let code = match key {
            NamedKey::Escape => KeyCode::Escape,
            NamedKey::Up => KeyCode::Up,
            NamedKey::Down => KeyCode::Down,
            NamedKey::Left => KeyCode::Left,
            NamedKey::Right => KeyCode::Right,
            NamedKey::Enter => KeyCode::Enter,
            NamedKey::Y => KeyCode::Char('y'),
            NamedKey::N => KeyCode::Char('n'),
            NamedKey::Digit1 => KeyCode::Char('1'),
            NamedKey::Digit2 => KeyCode::Char('2'),
            NamedKey::Digit3 => KeyCode::Char('3'),
            NamedKey::Digit4 => KeyCode::Char('4'),
            NamedKey::Digit5 => KeyCode::Char('5'),
            NamedKey::Digit6 => KeyCode::Char('6'),
            NamedKey::Digit7 => KeyCode::Char('7'),
            NamedKey::Digit8 => KeyCode::Char('8'),
            NamedKey::Digit9 => KeyCode::Char('9'),
        };
        Self::plain(code)
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_herdr_spec())
    }
}

impl Serialize for KeyChord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_herdr_spec())
    }
}

impl<'de> Deserialize<'de> for KeyChord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spec = String::deserialize(deserializer)?;
        Self::parse(&spec).map_err(serde::de::Error::custom)
    }
}

/// Provenance of a discovered Herdr terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalOrigin {
    /// Terminal created and labelled by Spawn (`spawn_owner` + `spawn_terminal`).
    SpawnManaged {
        /// Owning Spawn loop session label.
        owner_session: String,
        /// Spawn terminal name label (`spawn-...`).
        terminal_name: String,
        /// Optional workspace label carried by the terminal.
        workspace: Option<String>,
    },
    /// Terminal launched externally outside Spawn's own label ownership.
    External,
}

impl TerminalOrigin {
    /// Derives [`TerminalOrigin`] from a terminal's label map.
    #[must_use]
    pub fn from_labels(labels: &BTreeMap<String, String>) -> Self {
        match (
            labels.get(SPAWN_OWNER_LABEL),
            labels.get(SPAWN_TERMINAL_LABEL),
        ) {
            (Some(owner_session), Some(terminal_name)) => Self::SpawnManaged {
                owner_session: owner_session.clone(),
                terminal_name: terminal_name.clone(),
                workspace: labels.get(SPAWN_WORKSPACE_LABEL).cloned(),
            },
            _ => Self::External,
        }
    }
}

/// Live vs exited execution state reported by Herdr for a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TerminalLiveState {
    /// Terminal is currently running.
    Live,
    /// Terminal finished or lost its program.
    Exited {
        /// Process exit code when exited normally.
        code: Option<i32>,
        /// Signal number when terminated by signal.
        signal: Option<i32>,
        /// Herdr termination reason string.
        reason: Option<String>,
    },
}

/// Global terminal record discovered directly from `herdr-core` without label filtering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredTerminal {
    /// Terminal address on the core (`"t1"`) or remote core (`"n252/t1"`).
    pub address: String,
    /// Peer machine name when `address` carried a `machine/` prefix.
    pub machine: Option<String>,
    /// Optional human-assigned Herdr terminal name.
    pub name: Option<String>,
    /// Whether this terminal is Spawn-managed or externally launched.
    pub origin: TerminalOrigin,
    /// Current runtime liveness state.
    pub state: TerminalLiveState,
    /// Foreground process PID when live and known.
    pub pid: Option<u32>,
    /// Working directory of the terminal.
    pub cwd: String,
    /// Window / OSC terminal title.
    pub title: String,
    /// Original argv launched inside the terminal.
    pub argv: Vec<String>,
    /// Arbitrary metadata labels attached to the terminal.
    pub labels: BTreeMap<String, String>,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// Detected agent runtime name (`"claude"`, `"agy"`, `"codex"`, ...), if any.
    pub agent: Option<String>,
    /// Observed coarse agent activity status.
    pub agent_status: AgentStatus,
    /// Monotonic turn counter tracked by `herdr-core`.
    pub turn: u64,
    /// Whether `keep` keeps a shell open after the initial program drops.
    pub keep: bool,
}

/// How `herdr-core` `terminal.read` obtained a screen snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "detail", rename_all = "snake_case")]
pub enum ReadMode {
    /// Visible screen buffer only.
    Screen,
    /// Main screen combined with scrollback history.
    History,
    /// Alt-screen full-screen application captured by scrolling/stitching.
    Stitched,
    /// Unrecognized future `how` value preserved verbatim.
    Unknown(String),
}

impl ReadMode {
    /// Parses Herdr's `ReadResult.how` string.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "screen" => Self::Screen,
            "history" => Self::History,
            "stitched" => Self::Stitched,
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// A selectable choice detected on an interactive TUI screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiChoice {
    /// Explicit numeric shortcut (`1..=9`), when displayed.
    pub index: Option<u8>,
    /// Choice label text (ANSI sequences stripped and trimmed).
    pub label: String,
    /// Whether cursor marker (`❯`, `▸`, `▶`, `>`, `●`, `[*]`) or reverse video highlights it.
    pub selected: bool,
    /// Zero-based line index inside [`TuiSnapshot::plain_lines`].
    pub line_index: usize,
}

/// Structured, CAS-hashable snapshot of an interactive terminal screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiSnapshot {
    /// Target terminal address (`"t1"` or `"machine/t1"`).
    pub address: String,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// Source mode reported by `terminal.read`.
    pub read_mode: ReadMode,
    /// False if full requested scrollback or stitched history was truncated.
    pub complete: bool,
    /// Optional Herdr reason why alt-screen scrolling stopped (`"working"`, `"blocked"`, `"error"`).
    pub why: Option<String>,
    /// Current terminal title if non-empty.
    pub title: Option<String>,
    /// Screen lines with ANSI control sequences removed.
    pub plain_lines: Vec<String>,
    /// Detected agent runtime if any.
    pub agent: Option<String>,
    /// Observed coarse agent status.
    pub agent_status: AgentStatus,
    /// Matching witness rule name if a rule matched.
    pub rule: Option<String>,
    /// Whether a visible blocker/dialog was detected on screen.
    pub blocker_visible: bool,
    /// Parsed interactive menu/prompt choices detected on screen.
    pub choices: Vec<TuiChoice>,
    /// Deterministic 64-bit FNV-1a CAS digest over `plain_lines`.
    pub snapshot_hash: u64,
}

/// Raw screen input parameters passed to [`TuiSnapshot::from_raw`].
#[derive(Debug, Clone, Copy)]
pub struct RawTuiSnapshot<'a> {
    /// Target terminal address (`"t1"` or `"machine/t1"`).
    pub address: &'a str,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// Source mode reported by `terminal.read`.
    pub how: &'a str,
    /// False if full requested scrollback or stitched history was truncated.
    pub complete: bool,
    /// Optional Herdr reason why alt-screen scrolling stopped.
    pub why: Option<&'a str>,
    /// Optional window title.
    pub title: Option<&'a str>,
    /// Raw VT screen text captured from `terminal.read`.
    pub raw_text: &'a str,
    /// Detected agent runtime name if any.
    pub agent: Option<&'a str>,
    /// Observed coarse agent activity status.
    pub agent_status: AgentStatus,
    /// Matching witness rule name if any.
    pub rule: Option<&'a str>,
    /// Whether a visible blocker/dialog was detected on screen.
    pub blocker_visible: bool,
}

impl TuiSnapshot {
    /// Constructs a [`TuiSnapshot`] from raw `terminal.read` and witness fields.
    #[must_use]
    pub fn from_raw(input: RawTuiSnapshot<'_>) -> Self {
        let plain_lines: Vec<String> = input.raw_text.lines().map(strip_ansi_sequences).collect();
        let choices = parse_tui_choices(input.raw_text);
        let snapshot_hash = snapshot_hash(&plain_lines);
        Self {
            address: input.address.to_string(),
            cols: input.cols,
            rows: input.rows,
            read_mode: ReadMode::parse(input.how),
            complete: input.complete,
            why: input.why.map(str::to_string),
            title: input
                .title
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            plain_lines,
            agent: input.agent.map(str::to_string),
            agent_status: input.agent_status,
            rule: input.rule.map(str::to_string),
            blocker_visible: input.blocker_visible,
            choices,
            snapshot_hash,
        }
    }
}

/// Strips ANSI CSI, OSC, and 2-byte ESC control sequences from terminal output.
#[must_use]
pub fn strip_ansi_sequences(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == 0x1b {
            idx += 1;
            if idx >= bytes.len() {
                break;
            }
            match bytes[idx] {
                b'[' => {
                    idx += 1;
                    while idx < bytes.len() && !(0x40..=0x7e).contains(&bytes[idx]) {
                        idx += 1;
                    }
                    if idx < bytes.len() {
                        idx += 1;
                    }
                }
                b']' => {
                    idx += 1;
                    while idx < bytes.len() {
                        if bytes[idx] == 0x07 {
                            idx += 1;
                            break;
                        }
                        if bytes[idx] == 0x1b && idx + 1 < bytes.len() && bytes[idx + 1] == b'\\' {
                            idx += 2;
                            break;
                        }
                        idx += 1;
                    }
                }
                _ => {
                    idx += 1;
                }
            }
            continue;
        }
        let ch = raw[idx..].chars().next().unwrap_or('\0');
        let ch_len = ch.len_utf8();
        if ch != '\r' && !ch.is_control() {
            out.push(ch);
        }
        idx += ch_len;
    }
    out
}

/// Computes a deterministic 64-bit FNV-1a digest over normalized screen lines.
#[must_use]
pub fn snapshot_hash(lines: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in lines {
        for byte in line.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= u64::from(b'\n');
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Extracts interactive menu / prompt choices from raw VT screen text.
///
/// Recognizes selection prefixes (`❯`, `▸`, `▶`, `>`, `●`, `[*]`) and reverse-video
/// highlights (`\x1b[7m`) alongside explicit numbered choices (`1. `, `1) `, `[1] `).
#[must_use]
pub fn parse_tui_choices(raw_text: &str) -> Vec<TuiChoice> {
    let mut choices = Vec::new();
    for (line_index, raw_line) in raw_text.lines().enumerate() {
        let has_reverse_video = contains_reverse_video(raw_line);
        let plain = strip_ansi_sequences(raw_line);
        let trimmed = plain.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (has_marker, after_marker) = strip_selection_marker(trimmed);
        if let Some((index, rest)) = parse_numbered_choice(after_marker) {
            if !rest.is_empty() {
                choices.push(TuiChoice {
                    index: Some(index),
                    label: rest.to_string(),
                    selected: has_marker || has_reverse_video,
                    line_index,
                });
            }
        } else if has_marker && !after_marker.is_empty() {
            choices.push(TuiChoice {
                index: None,
                label: after_marker.to_string(),
                selected: true,
                line_index,
            });
        }
    }
    choices
}

fn contains_reverse_video(raw_line: &str) -> bool {
    let bytes = raw_line.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'm' {
                let sgr = &raw_line[start..end];
                if sgr.split(';').any(|code| code == "7") {
                    return true;
                }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    false
}

fn strip_selection_marker(trimmed: &str) -> (bool, &str) {
    for marker in ["❯", "▸", "▶", "●", "[*]", ">"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return (true, rest.trim_start());
        }
    }
    (false, trimmed)
}

fn parse_numbered_choice(text: &str) -> Option<(u8, &str)> {
    if let Some(rest) = text.strip_prefix('[') {
        let digit = rest.as_bytes().first().copied()?;
        if (b'1'..=b'9').contains(&digit) && rest.as_bytes().get(1) == Some(&b']') {
            return Some((digit - b'0', rest[2..].trim_start()));
        }
        return None;
    }
    let first = text.as_bytes().first().copied()?;
    if !(b'1'..=b'9').contains(&first) {
        return None;
    }
    let sep = text.as_bytes().get(1).copied()?;
    if sep == b'.' || sep == b')' {
        let after = &text[2..];
        if after.starts_with(char::is_whitespace) {
            return Some((first - b'0', after.trim_start()));
        }
    }
    None
}

/// Semantic delivery policy for human / supervisor / peer messages to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendDeliveryMode {
    /// Deliver cleanly if idle, else queue or follow active delivery policy.
    Auto,
    /// Inject/steer mid-turn immediately where supported.
    Steer,
    /// Queue until the current turn finishes.
    Queue,
    /// Interrupt current execution before sending.
    Interrupt,
}

/// Herdr-level precondition passed via `terminal.send`'s `if` parameter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendPrecondition {
    /// Send regardless of `idle`/`working` status (subject to `force` when blocked).
    #[default]
    Any,
    /// Require the program status to currently equal `"idle"`.
    OnlyIfIdle,
    /// Require the program status to currently equal `"working"`.
    OnlyIfWorking,
}

impl SendPrecondition {
    /// Wire value for `herdr-core` `SendParams::only_if` (`"if"`).
    #[must_use]
    pub const fn as_herdr_if(self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::OnlyIfIdle => Some("idle"),
            Self::OnlyIfWorking => Some("working"),
        }
    }
}

/// Declarative action executed against a live terminal through Herdr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TuiAction {
    /// Submit pasted text + Enter.
    SendText {
        /// Text to paste and submit.
        text: String,
        /// Send even when the terminal reports `blocked`.
        force: bool,
        /// Optional status precondition (`idle` or `working`).
        precondition: SendPrecondition,
        /// Wait for the turn triggered by this send to settle.
        wait: bool,
    },
    /// Send a sequence of VT key chords in a single `terminal.keys` RPC.
    SendKeys {
        /// Non-empty sequence of VT key chords.
        keys: Vec<KeyChord>,
    },
    /// Interrupt the foreground program (`terminal.interrupt`).
    Interrupt {
        /// Optional observation timeout in milliseconds.
        timeout_ms: Option<u64>,
    },
    /// Event-driven wait until the terminal leaves `working` or emits matching output.
    WaitSettled {
        /// Optional previously observed turn counter for fast-path skip.
        after_turn: Option<u64>,
        /// Optional plain-text substring trigger in subsequent terminal output.
        output_contains: Option<String>,
        /// Wait timeout in milliseconds.
        timeout_ms: u64,
    },
}

/// Result classification returned by `herdr-core` `terminal.interrupt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptOutcome {
    /// Program left `working` or returned to its prompt.
    Stopped,
    /// Process exited as a result of interrupting.
    Exited,
    /// Program remained `working` past the observation window.
    StillWorking,
    /// Program does not report status; interrupt key was delivered best-effort.
    Unknown,
}

impl InterruptOutcome {
    /// Parses Herdr's `InterruptResult::outcome` wire string.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "stopped" => Self::Stopped,
            "exited" => Self::Exited,
            "still_working" => Self::StillWorking,
            _ => Self::Unknown,
        }
    }
}

/// Typed outcome of a `terminal.interrupt` RPC call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptReport {
    /// Classified outcome of the interrupt.
    pub outcome: InterruptOutcome,
    /// Key spec actually sent by Herdr (`"esc"` or `"ctrl+c"`).
    pub key_sent: Option<String>,
}

/// Cause explaining why [`WaitOutcome`] completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "raw", rename_all = "snake_case")]
pub enum WaitReason {
    /// Status transitioned out of `working` (for example `idle`, `done`, `blocked`, `error`).
    Status,
    /// Terminal process exited.
    Exit,
    /// Output produced during the wait matched the requested substring.
    Output,
    /// Timeout expired before settling (`ended = false`).
    Timeout,
    /// Terminal never transitioned to `working` after a tracked send.
    NotStarted,
    /// Child-wait aggregation returned (`--children`).
    Children,
    /// Wait woke early because another caller messaged the terminal.
    Message,
    /// Unrecognized future `reason` value preserved verbatim.
    Other(String),
}

impl WaitReason {
    /// Parses Herdr's `WaitResult::reason` wire string.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "status" => Self::Status,
            "exit" => Self::Exit,
            "output" => Self::Output,
            "timeout" => Self::Timeout,
            "not_started" => Self::NotStarted,
            "children" => Self::Children,
            "message" => Self::Message,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Typed result of an event-driven `terminal.wait` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitOutcome {
    /// Terminal address reported by Herdr.
    pub terminal: String,
    /// True if settled (`!timeout`); false on timeout check-in.
    pub ended: bool,
    /// Reason the wait completed.
    pub reason: WaitReason,
    /// Current OSC 7501 status word if reported.
    pub status: Option<String>,
    /// Terminal turn counter when the wait returned.
    pub turn: u64,
    /// Process exit code when `reason == Exit`.
    pub exit_code: Option<i32>,
    /// Terminating signal when `reason == Exit`.
    pub signal: Option<i32>,
    /// Sender address when `reason == Message`.
    pub from: Option<String>,
}

impl Serialize for AgentStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let word = match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        };
        serializer.serialize_str(word)
    }
}

impl<'de> Deserialize<'de> for AgentStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        Ok(Self::from_herdr(&word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chord_parses_formats_and_roundtrips_all_named_keys() {
        for (input, expected_spec) in [
            ("ctrl+c", "ctrl+c"),
            ("Control+Shift+Tab", "ctrl+shift+tab"),
            ("alt+shift+tab", "alt+shift+tab"),
        ] {
            let chord = KeyChord::parse(input).expect("valid chord");
            assert_eq!(chord.to_herdr_spec(), expected_spec);
            let json = serde_json::to_string(&chord).expect("serialize");
            let parsed: KeyChord = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, chord);
        }

        for (raw, want) in [
            ("shift+tab", "shift+tab"),
            ("alt+enter", "alt+enter"),
            ("pgup", "pageup"),
            ("pgdn", "pagedown"),
            ("del", "delete"),
            ("F12", "f12"),
            ("backslash", "\\"),
            ("digit3", "3"),
            ("home", "home"),
            ("end", "end"),
            ("space", "space"),
        ] {
            let parsed = KeyChord::parse(raw).expect(raw);
            assert_eq!(parsed.to_herdr_spec(), want);
        }

        for key in NamedKey::ALL {
            let chord = KeyChord::from(key);
            let reparsed = KeyChord::parse(&chord.to_herdr_spec()).expect("named key reparses");
            assert_eq!(reparsed, chord);
        }
    }

    #[test]
    fn key_chord_rejects_whitespace_duplicates_and_unknown_tokens() {
        for bad in [
            "",
            " ctrl+c",
            "ctrl+c ",
            "ctrl + c",
            "ctrl+",
            "+c",
            "ctrl+ctrl+c",
            "hyper+x",
            "f0",
            "f13",
            "digit0",
            "hello",
        ] {
            assert!(KeyChord::parse(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn terminal_origin_distinguishes_spawn_managed_vs_external() {
        let empty = BTreeMap::new();
        assert_eq!(
            TerminalOrigin::from_labels(&empty),
            TerminalOrigin::External
        );

        let mut partial = BTreeMap::new();
        partial.insert(SPAWN_OWNER_LABEL.to_string(), "loop-1".to_string());
        assert_eq!(
            TerminalOrigin::from_labels(&partial),
            TerminalOrigin::External
        );

        let mut managed = partial;
        managed.insert(SPAWN_TERMINAL_LABEL.to_string(), "spawn-exec_1".to_string());
        managed.insert(
            SPAWN_WORKSPACE_LABEL.to_string(),
            "git:/tmp/repo".to_string(),
        );
        assert_eq!(
            TerminalOrigin::from_labels(&managed),
            TerminalOrigin::SpawnManaged {
                owner_session: "loop-1".to_string(),
                terminal_name: "spawn-exec_1".to_string(),
                workspace: Some("git:/tmp/repo".to_string()),
            }
        );
    }

    #[test]
    fn snapshot_strips_ansi_computes_deterministic_hash_and_parses_choices() {
        let raw = concat!(
            "\x1b[2J\x1b[1;1HSelect action:\r\n",
            "  \x1b[36m❯\x1b[0m 1. Allow tool execution\r\n",
            "    2) Deny once\r\n",
            "    \x1b[7m[3] Always allow in workspace\x1b[0m\r\n",
            "  \x1b[1m▸ Resume previous session\x1b[0m\r\n",
        );
        let snap1 = TuiSnapshot::from_raw(RawTuiSnapshot {
            address: "t1",
            cols: 120,
            rows: 40,
            how: "screen",
            complete: true,
            why: None,
            title: Some("claude"),
            raw_text: raw,
            agent: Some("claude"),
            agent_status: AgentStatus::Blocked,
            rule: Some("permission_prompt"),
            blocker_visible: true,
        });
        let snap2 = TuiSnapshot::from_raw(RawTuiSnapshot {
            address: "t1",
            cols: 120,
            rows: 40,
            how: "screen",
            complete: true,
            why: None,
            title: Some("claude"),
            raw_text: raw,
            agent: Some("claude"),
            agent_status: AgentStatus::Blocked,
            rule: Some("permission_prompt"),
            blocker_visible: true,
        });
        assert_eq!(snap1.snapshot_hash, snap2.snapshot_hash);
        assert_eq!(snap1.read_mode, ReadMode::Screen);
        assert_eq!(snap1.choices.len(), 4);
        assert_eq!(
            snap1.choices[0],
            TuiChoice {
                index: Some(1),
                label: "Allow tool execution".to_string(),
                selected: true,
                line_index: 1,
            }
        );
        assert_eq!(
            snap1.choices[1],
            TuiChoice {
                index: Some(2),
                label: "Deny once".to_string(),
                selected: false,
                line_index: 2,
            }
        );
        assert_eq!(
            snap1.choices[2],
            TuiChoice {
                index: Some(3),
                label: "Always allow in workspace".to_string(),
                selected: true,
                line_index: 3,
            }
        );
        assert_eq!(
            snap1.choices[3],
            TuiChoice {
                index: None,
                label: "Resume previous session".to_string(),
                selected: true,
                line_index: 4,
            }
        );

        let altered_raw = raw.replace("Allow tool", "Reject tool");
        let altered = TuiSnapshot::from_raw(RawTuiSnapshot {
            address: "t1",
            cols: 120,
            rows: 40,
            how: "screen",
            complete: true,
            why: None,
            title: Some("claude"),
            raw_text: &altered_raw,
            agent: Some("claude"),
            agent_status: AgentStatus::Blocked,
            rule: Some("permission_prompt"),
            blocker_visible: true,
        });
        assert_ne!(snap1.snapshot_hash, altered.snapshot_hash);
    }

    #[test]
    fn tui_action_serde_roundtrips() {
        let actions = [
            TuiAction::SendText {
                text: "continue".to_string(),
                force: true,
                precondition: SendPrecondition::OnlyIfIdle,
                wait: true,
            },
            TuiAction::SendKeys {
                keys: vec![
                    KeyChord::parse("down").expect("down"),
                    KeyChord::parse("enter").expect("enter"),
                ],
            },
            TuiAction::Interrupt {
                timeout_ms: Some(5000),
            },
            TuiAction::WaitSettled {
                after_turn: Some(3),
                output_contains: Some("done".to_string()),
                timeout_ms: 15_000,
            },
        ];
        for action in actions {
            let encoded = serde_json::to_string(&action).expect("serialize action");
            let decoded: TuiAction = serde_json::from_str(&encoded).expect("deserialize action");
            assert_eq!(decoded, action);
        }
        assert_eq!(SendDeliveryMode::Steer, SendDeliveryMode::Steer);
    }
}
