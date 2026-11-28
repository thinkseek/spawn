//! Pure Agent Runtime TUI & zero-invasion Herdr control domain types.
//!
//! Models unfiltered global terminal discovery, closed VT/xterm key chords,
//! structured TUI snapshots (styled VT or plain-text fallback) with optimistic
//! guard digests, and a single closed [`TuiAction`] / [`TuiActionOutcome`]
//! control algebra without performing any IO or reading clocks.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::command::NamedKey;
use crate::port::AgentStatus;
use crate::screen::strip_ansi;

/// Label key written by Spawn's managed `HerdrCoreHost` on owned terminals
/// (aligned with `crates/spawn-host/src/herdr_core.rs`).
pub const SPAWN_OWNER_LABEL: &str = "spawn_owner";

/// Label key holding the logical `TerminalName` on Spawn-owned terminals
/// (aligned with `crates/spawn-host/src/herdr_core.rs`).
pub const SPAWN_TERMINAL_LABEL: &str = "spawn_terminal";

/// Label key holding the `WorkspaceId` string on Spawn-owned terminals
/// (aligned with `crates/spawn-host/src/herdr_core.rs`).
pub const SPAWN_WORKSPACE_LABEL: &str = "spawn_workspace";

/// Reserved label prefix protecting Spawn-managed terminal ownership metadata.
pub const SPAWN_RESERVED_LABEL_PREFIX: &str = "spawn_";

/// Returns `true` if `key` is reserved for Spawn-managed Herdr ownership/metadata
/// (case-insensitive `spawn_` prefix after trimming whitespace).
pub fn is_reserved_spawn_label(key: &str) -> bool {
    key.trim()
        .as_bytes()
        .get(..SPAWN_RESERVED_LABEL_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SPAWN_RESERVED_LABEL_PREFIX.as_bytes()))
}

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
    pub const fn none() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
        }
    }
}

/// Closed set of VT function keys supported by `herdr-core::keys::encode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionKey {
    /// `f1`
    F1,
    /// `f2`
    F2,
    /// `f3`
    F3,
    /// `f4`
    F4,
    /// `f5`
    F5,
    /// `f6`
    F6,
    /// `f7`
    F7,
    /// `f8`
    F8,
    /// `f9`
    F9,
    /// `f10`
    F10,
    /// `f11`
    F11,
    /// `f12`
    F12,
}

impl FunctionKey {
    /// Canonical lowercase key specification string (`"f1"`..=`"f12"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F1 => "f1",
            Self::F2 => "f2",
            Self::F3 => "f3",
            Self::F4 => "f4",
            Self::F5 => "f5",
            Self::F6 => "f6",
            Self::F7 => "f7",
            Self::F8 => "f8",
            Self::F9 => "f9",
            Self::F10 => "f10",
            Self::F11 => "f11",
            Self::F12 => "f12",
        }
    }

    fn from_lower_token(lower: &str) -> Option<Self> {
        match lower {
            "f1" => Some(Self::F1),
            "f2" => Some(Self::F2),
            "f3" => Some(Self::F3),
            "f4" => Some(Self::F4),
            "f5" => Some(Self::F5),
            "f6" => Some(Self::F6),
            "f7" => Some(Self::F7),
            "f8" => Some(Self::F8),
            "f9" => Some(Self::F9),
            "f10" => Some(Self::F10),
            "f11" => Some(Self::F11),
            "f12" => Some(Self::F12),
            _ => None,
        }
    }
}

/// Validated printable ASCII key character supported by `herdr-core::keys::encode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChar(char);

impl KeyChar {
    /// Validates and wraps a printable ASCII key (`a..=z`, `0..=9`, or
    /// `` ` \ [ ] , = - . ' ; / ``).
    pub fn new(ch: char) -> Result<Self, String> {
        if is_supported_key_char(ch) {
            Ok(Self(ch))
        } else {
            Err(format!("unsupported VT key char {ch:?}"))
        }
    }

    /// Underlying validated ASCII character.
    pub const fn as_char(self) -> char {
        self.0
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
    Function(FunctionKey),
    /// Validated printable ASCII key supported by Herdr's VT encoder.
    Char(KeyChar),
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
            && let Ok(key_char) = KeyChar::new(ch)
        {
            return Ok(Self::Char(key_char));
        }
        if let Some(fkey) = FunctionKey::from_lower_token(&lower) {
            return Ok(Self::Function(fkey));
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
            "backslash" => Ok(Self::Char(KeyChar('\\'))),
            d if d.starts_with("digit") && d.len() == 6 => {
                let ch = d.as_bytes()[5] as char;
                if ch.is_ascii_digit() {
                    Ok(Self::Char(KeyChar(ch)))
                } else {
                    Err(format!("unsupported digit key {token:?}"))
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
            Self::Function(fkey) => out.push_str(fkey.as_str()),
            Self::Char(ch) => out.push(ch.as_char()),
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
            NamedKey::Y => KeyCode::Char(KeyChar('y')),
            NamedKey::N => KeyCode::Char(KeyChar('n')),
            NamedKey::Digit1 => KeyCode::Char(KeyChar('1')),
            NamedKey::Digit2 => KeyCode::Char(KeyChar('2')),
            NamedKey::Digit3 => KeyCode::Char(KeyChar('3')),
            NamedKey::Digit4 => KeyCode::Char(KeyChar('4')),
            NamedKey::Digit5 => KeyCode::Char(KeyChar('5')),
            NamedKey::Digit6 => KeyCode::Char(KeyChar('6')),
            NamedKey::Digit7 => KeyCode::Char(KeyChar('7')),
            NamedKey::Digit8 => KeyCode::Char(KeyChar('8')),
            NamedKey::Digit9 => KeyCode::Char(KeyChar('9')),
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
        Self::parse(&spec).map_err(DeError::custom)
    }
}

/// Validated non-empty sequence of [`KeyChord`]s sent in a single `terminal.keys` RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChordSequence(Vec<KeyChord>);

impl KeyChordSequence {
    /// Wraps `keys`, failing if `keys` is empty.
    pub fn new(keys: Vec<KeyChord>) -> Result<Self, String> {
        if keys.is_empty() {
            return Err("key chord sequence must contain at least one key".to_string());
        }
        Ok(Self(keys))
    }

    /// Constructs a one-key sequence.
    pub fn single(key: KeyChord) -> Self {
        Self(vec![key])
    }

    /// Ordered slice of validated key chords.
    pub fn as_slice(&self) -> &[KeyChord] {
        &self.0
    }
}

impl Serialize for KeyChordSequence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for KeyChordSequence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let keys = Vec::<KeyChord>::deserialize(deserializer)?;
        Self::new(keys).map_err(DeError::custom)
    }
}

/// Validated terminal address (`"<terminal>"` or `"<machine>/<terminal>"`).
///
/// Rejects empty strings, whitespace or control characters, path traversal
/// segments (`"."`, `".."`), leading/trailing/double slashes, and addresses with
/// more than two `'/'`-separated segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct TerminalAddress(String);

impl TerminalAddress {
    /// Parses and validates a terminal address fail-closed.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty()
            || raw.starts_with('/')
            || raw.ends_with('/')
            || raw.contains("//")
            || raw.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err(format!("invalid terminal address {raw:?}"));
        }
        let mut segments = raw.split('/');
        let first = segments
            .next()
            .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
            .ok_or_else(|| format!("invalid terminal address {raw:?}"))?;
        if let Some(second) = segments.next() {
            if second.is_empty() || second == "." || second == ".." || segments.next().is_some() {
                return Err(format!("invalid terminal address {raw:?}"));
            }
            let _ = first;
        }
        Ok(Self(raw.to_string()))
    }

    /// Full validated address string (`"t1"` or `"n252/t1"`).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Optional federated peer machine prefix (`"n252"` in `"n252/t1"`).
    pub fn machine(&self) -> Option<&str> {
        self.0.split_once('/').map(|(machine, _)| machine)
    }

    /// Local terminal identifier (`"t1"` in both `"t1"` and `"n252/t1"`).
    pub fn terminal(&self) -> &str {
        self.0
            .split_once('/')
            .map_or(self.0.as_str(), |(_, term)| term)
    }
}

impl fmt::Display for TerminalAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TerminalAddress {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TerminalAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(DeError::custom)
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

/// Mutually exclusive reason a terminal process stopped running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExitCause {
    /// Process exited normally with a numeric status code.
    Code {
        /// Exit status code reported by the OS.
        code: i32,
    },
    /// Process terminated due to a signal.
    Signal {
        /// Signal number reported by the OS/host.
        signal: i32,
    },
    /// Process ended without a numeric exit code or signal (e.g. `holder_lost` or `killed`).
    Lost {
        /// Optional Herdr termination reason string.
        reason: Option<String>,
    },
}

impl ExitCause {
    /// Parses Herdr's mutually exclusive `exit_code` / `signal` / `reason` fields fail-closed.
    pub fn from_herdr(
        code: Option<i32>,
        signal: Option<i32>,
        reason: Option<String>,
    ) -> Result<Self, String> {
        match (code, signal) {
            (Some(exit_code), Some(sig)) => Err(format!(
                "contradictory Herdr exit state: both exit_code={exit_code} and signal={sig} are set"
            )),
            (Some(exit_code), None) => Ok(Self::Code { code: exit_code }),
            (None, Some(sig)) => Ok(Self::Signal { signal: sig }),
            (None, None) => Ok(Self::Lost { reason }),
        }
    }
}

/// Live vs exited execution state reported by Herdr for a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalLiveState {
    /// Terminal is currently running.
    Live {
        /// Foreground process PID when known.
        pid: Option<u32>,
    },
    /// Terminal finished or lost its program.
    Exited {
        /// Structured mutually exclusive exit cause.
        cause: ExitCause,
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
    /// Current runtime liveness state (with `pid` nested in `Live`).
    pub state: TerminalLiveState,
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
    /// Auto-discovered native session attachment when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::agent_events::SessionAttachment>,
    /// Host-observed process tree, memory, and session discovery telemetry when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<crate::agent_events::TerminalTelemetrySummary>,
    /// Monotonic turn counter tracked by `herdr-core`.
    pub turn: u64,
    /// Whether `keep` keeps a shell open after the initial program drops.
    pub keep: bool,
}

impl DiscoveredTerminal {
    /// Foreground process PID when [`TerminalLiveState::Live`], otherwise `None`.
    pub const fn pid(&self) -> Option<u32> {
        match self.state {
            TerminalLiveState::Live { pid } => pid,
            TerminalLiveState::Exited { .. } => None,
        }
    }
}

/// How a snapshot's underlying terminal screen buffer was captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "detail",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReadMode {
    /// Visible screen buffer only.
    Screen,
    /// Main screen combined with scrollback history.
    History,
    /// Alt-screen full-screen application captured by scrolling/stitching.
    Stitched,
    /// Unrecognized future `how` value preserved verbatim.
    Other(String),
}

impl ReadMode {
    /// Parses Herdr's `ReadResult.how` string.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "screen" => Self::Screen,
            "history" => Self::History,
            "stitched" => Self::Stitched,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Zero-based cursor coordinates reported by styled VT capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorPos {
    /// Zero-based screen row.
    pub row: u16,
    /// Zero-based screen column.
    pub col: u16,
}

/// Visual fidelity preserved inside a [`TuiSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fidelity", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotFidelity {
    /// Captured via framed `terminal.attach(observe: true)` retaining SGR styles,
    /// reverse-video highlights, cursor coordinates, alternate-screen mode, and
    /// canonicalized styled lines.
    StyledVt {
        /// Zero-based terminal cursor coordinates when present in paint output.
        cursor: Option<CursorPos>,
        /// Whether the alternate screen buffer was active at capture time.
        alt_screen: bool,
        /// Canonicalized visible lines preserving SGR sequences.
        styled_lines: Vec<String>,
    },
    /// Captured via `terminal.read`, which strips SGR styles and cursor state.
    PlainTextOnly {
        /// Source mode of the plain-text capture.
        read_mode: ReadMode,
        /// False if full requested scrollback or stitched history was truncated.
        complete: bool,
        /// Optional Herdr reason why alt-screen scrolling stopped (`"working"`, `"blocked"`, `"error"`).
        why: Option<String>,
    },
}

/// Optimistic pre-flight screen digest returned by [`TuiSnapshot::guard`].
///
/// Covers terminal address, dimensions, [`TerminalLiveState`], [`SnapshotFidelity`],
/// plain lines, and parsed [`TuiChoice`] selection state. Serialized as canonical
/// `0x`-prefixed lowercase 16-digit hex strings so JSON/JS callers retain all 64 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScreenGuard(u64);

impl ScreenGuard {
    /// Wraps a raw FNV-1a snapshot digest.
    pub const fn from_raw(digest: u64) -> Self {
        Self(digest)
    }

    /// Parses a screen guard from `0x`/`0X`-prefixed hex (`1..=16` hex digits),
    /// 16-digit bare hex, or decimal `u64`, failing closed on empty/whitespace or overflow.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() || raw.chars().any(char::is_whitespace) {
            return Err(format!("invalid screen guard {raw:?}"));
        }
        if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
            if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("invalid hex screen guard {raw:?}"));
            }
            return u64::from_str_radix(hex, 16)
                .map(Self)
                .map_err(|err| format!("invalid hex screen guard {raw:?}: {err}"));
        }
        if raw.bytes().all(|b| b.is_ascii_digit()) {
            return raw
                .parse::<u64>()
                .map(Self)
                .map_err(|err| format!("invalid screen guard {raw:?}: {err}"));
        }
        if raw.len() == 16 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return u64::from_str_radix(raw, 16)
                .map(Self)
                .map_err(|err| format!("invalid hex screen guard {raw:?}: {err}"));
        }
        Err(format!("invalid screen guard {raw:?}"))
    }

    /// Underlying 64-bit FNV-1a digest.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ScreenGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl Serialize for ScreenGuard {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ScreenGuard {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct GuardVisitor;

        impl serde::de::Visitor<'_> for GuardVisitor {
            type Value = ScreenGuard;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical hex/decimal ScreenGuard string or u64")
            }

            fn visit_u64<E: DeError>(self, value: u64) -> Result<Self::Value, E> {
                Ok(ScreenGuard::from_raw(value))
            }

            fn visit_str<E: DeError>(self, value: &str) -> Result<Self::Value, E> {
                ScreenGuard::parse(value).map_err(DeError::custom)
            }
        }

        deserializer.deserialize_any(GuardVisitor)
    }
}

/// A selectable choice detected on an interactive TUI screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiChoice {
    /// Explicit numeric shortcut (`1..=9`), when displayed.
    pub index: Option<u8>,
    /// Choice label text (ANSI sequences stripped and trimmed).
    pub label: String,
    /// Whether cursor marker (`❯`, `▸`, `▶`, `>`, `●`, `[*]`) or SGR reverse-video highlights it.
    pub selected: bool,
    /// Zero-based line index inside [`TuiSnapshot::plain_lines`].
    pub line_index: usize,
}

/// Observer metadata attached to a [`TuiSnapshot`] at construction time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotObservation {
    /// Detected agent runtime name if any.
    pub agent: Option<String>,
    /// Observed coarse agent activity status.
    pub agent_status: AgentStatus,
    /// Matching witness rule name if any.
    pub rule: Option<String>,
    /// Whether a visible blocker/dialog was detected on screen.
    pub blocker_visible: bool,
}

impl Default for SnapshotObservation {
    fn default() -> Self {
        Self {
            agent: None,
            agent_status: AgentStatus::Unknown,
            rule: None,
            blocker_visible: false,
        }
    }
}

/// Raw `terminal.read` plain-text parameters passed to [`TuiSnapshot::from_plain_text`].
#[derive(Debug, Clone)]
pub struct RawPlainSnapshot<'a> {
    /// Target terminal address (`"t1"` or `"machine/t1"`).
    pub address: &'a str,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// Current runtime liveness state reported by `terminal.get`.
    pub live_state: TerminalLiveState,
    /// Source mode reported by `terminal.read`.
    pub how: &'a str,
    /// False if full requested scrollback or stitched history was truncated.
    pub complete: bool,
    /// Optional Herdr reason why alt-screen scrolling stopped.
    pub why: Option<&'a str>,
    /// Optional window title.
    pub title: Option<&'a str>,
    /// Text returned by `terminal.read`.
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

/// Raw framed VT paint parameters passed to [`TuiSnapshot::from_styled_vt`] and
/// [`TuiSnapshot::from_styled_vt_observed`].
#[derive(Debug, Clone)]
pub struct RawStyledSnapshot<'a> {
    /// Target terminal address (`"t1"` or `"machine/t1"`).
    pub address: &'a str,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
    /// Current runtime liveness state reported by `terminal.get`.
    pub live_state: TerminalLiveState,
    /// Optional window title.
    pub title: Option<&'a str>,
    /// Framed `first_paint` VT stream up to/including DEC mode 9001 marker.
    pub raw_vt: &'a str,
}

/// Integrity-checked structured snapshot of an interactive terminal screen.
///
/// All fields are private so [`Self::guard`] and [`Self::fidelity`] stay
/// unforgeable after construction. Deserialization rejects tampered
/// `guard` or `snapshot_hash` values and unknown wire fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiSnapshot {
    address: String,
    cols: u16,
    rows: u16,
    live_state: TerminalLiveState,
    title: Option<String>,
    fidelity: SnapshotFidelity,
    plain_lines: Vec<String>,
    agent: Option<String>,
    agent_status: AgentStatus,
    rule: Option<String>,
    blocker_visible: bool,
    choices: Vec<TuiChoice>,
    snapshot_hash: u64,
}

impl TuiSnapshot {
    /// Builds a [`TuiSnapshot`] from framed `terminal.attach(observe: true)` VT output
    /// together with pre-computed [`SnapshotObservation`] metadata.
    ///
    /// Preserves SGR styling (including reverse-video and color selection bars),
    /// cursor coordinates, alt-screen state, and canonicalized styled lines inside
    /// [`SnapshotFidelity::StyledVt`].
    pub fn from_styled_vt(input: RawStyledSnapshot<'_>, observation: SnapshotObservation) -> Self {
        Self::from_styled_vt_observed(input, |_| observation)
    }

    /// Builds a [`TuiSnapshot`] from framed VT output while computing witness
    /// [`SnapshotObservation`] from the stripped `plain_lines` without mutating fields later.
    pub fn from_styled_vt_observed(
        input: RawStyledSnapshot<'_>,
        observe: impl FnOnce(&[String]) -> SnapshotObservation,
    ) -> Self {
        let (styled_lines, cursor, alt_screen) = parse_herdr_first_paint(input.raw_vt);
        let plain_lines: Vec<String> = styled_lines
            .iter()
            .map(|line| clean_plain_line(line))
            .collect();
        let choices = parse_tui_choices_from_lines(&styled_lines);
        let fidelity = SnapshotFidelity::StyledVt {
            cursor,
            alt_screen,
            styled_lines,
        };
        let obs = observe(&plain_lines);
        let snapshot_hash = hash_snapshot_parts(
            input.address,
            input.cols,
            input.rows,
            &input.live_state,
            &fidelity,
            &plain_lines,
            &choices,
        );
        Self {
            address: input.address.to_string(),
            cols: input.cols,
            rows: input.rows,
            live_state: input.live_state,
            title: input
                .title
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            fidelity,
            plain_lines,
            agent: obs.agent,
            agent_status: obs.agent_status,
            rule: obs.rule,
            blocker_visible: obs.blocker_visible,
            choices,
            snapshot_hash,
        }
    }

    /// Builds a plain-text-only [`TuiSnapshot`] from `terminal.read`.
    pub fn from_plain_text(input: RawPlainSnapshot<'_>) -> Self {
        let plain_lines: Vec<String> = input.raw_text.lines().map(clean_plain_line).collect();
        let choices = parse_tui_choices_from_lines(&plain_lines);
        let fidelity = SnapshotFidelity::PlainTextOnly {
            read_mode: ReadMode::parse(input.how),
            complete: input.complete,
            why: input.why.map(str::to_string),
        };
        let snapshot_hash = hash_snapshot_parts(
            input.address,
            input.cols,
            input.rows,
            &input.live_state,
            &fidelity,
            &plain_lines,
            &choices,
        );
        Self {
            address: input.address.to_string(),
            cols: input.cols,
            rows: input.rows,
            live_state: input.live_state,
            title: input
                .title
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            fidelity,
            plain_lines,
            agent: input.agent.map(str::to_string),
            agent_status: input.agent_status,
            rule: input.rule.map(str::to_string),
            blocker_visible: input.blocker_visible,
            choices,
            snapshot_hash,
        }
    }

    /// Target terminal address (`"t1"` or `"machine/t1"`).
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Terminal width in columns.
    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// Terminal height in rows.
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Current runtime liveness state captured alongside the screen.
    pub const fn live_state(&self) -> &TerminalLiveState {
        &self.live_state
    }

    /// Current terminal title if non-empty.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Visual fidelity preserved by this snapshot (owning any styled VT or plain-text metadata).
    pub const fn fidelity(&self) -> &SnapshotFidelity {
        &self.fidelity
    }

    /// Canonicalized visible lines preserving SGR sequences (`Some` iff [`SnapshotFidelity::StyledVt`]).
    pub fn styled_lines(&self) -> Option<&[String]> {
        match &self.fidelity {
            SnapshotFidelity::StyledVt { styled_lines, .. } => Some(styled_lines.as_slice()),
            SnapshotFidelity::PlainTextOnly { .. } => None,
        }
    }

    /// Source mode of the capture (`Screen` for framed `StyledVt`).
    pub fn read_mode(&self) -> ReadMode {
        match &self.fidelity {
            SnapshotFidelity::StyledVt { .. } => ReadMode::Screen,
            SnapshotFidelity::PlainTextOnly { read_mode, .. } => read_mode.clone(),
        }
    }

    /// True if capture was complete without truncation.
    pub const fn complete(&self) -> bool {
        match &self.fidelity {
            SnapshotFidelity::StyledVt { .. } => true,
            SnapshotFidelity::PlainTextOnly { complete, .. } => *complete,
        }
    }

    /// Optional Herdr reason why alt-screen scrolling stopped (`"working"`, `"blocked"`, `"error"`).
    pub fn why(&self) -> Option<&str> {
        match &self.fidelity {
            SnapshotFidelity::StyledVt { .. } => None,
            SnapshotFidelity::PlainTextOnly { why, .. } => why.as_deref(),
        }
    }

    /// Visible screen lines with ANSI control sequences stripped.
    pub fn plain_lines(&self) -> &[String] {
        &self.plain_lines
    }

    /// Detected agent runtime if any.
    pub fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    /// Observed coarse agent status.
    pub const fn agent_status(&self) -> AgentStatus {
        self.agent_status
    }

    /// Matching witness rule name if a rule matched.
    pub fn rule(&self) -> Option<&str> {
        self.rule.as_deref()
    }

    /// Whether a visible blocker/dialog was detected on screen.
    pub const fn blocker_visible(&self) -> bool {
        self.blocker_visible
    }

    /// Parsed interactive menu/prompt choices detected on screen.
    pub fn choices(&self) -> &[TuiChoice] {
        &self.choices
    }

    /// Deterministic 64-bit FNV-1a snapshot digest covering screen structure, fidelity, and choices.
    pub const fn snapshot_hash(&self) -> u64 {
        self.snapshot_hash
    }

    /// Typed optimistic pre-flight guard token derived from [`Self::snapshot_hash`].
    pub const fn guard(&self) -> ScreenGuard {
        ScreenGuard(self.snapshot_hash)
    }

    /// Recomputes the canonical snapshot digest from this snapshot's fields.
    pub fn compute_hash(&self) -> u64 {
        hash_snapshot_parts(
            &self.address,
            self.cols,
            self.rows,
            &self.live_state,
            &self.fidelity,
            &self.plain_lines,
            &self.choices,
        )
    }

    /// Returns `true` when visual selection state is reliably observable for guarded writes.
    ///
    /// Strictly returns `true` only for [`SnapshotFidelity::StyledVt`]. Any
    /// [`SnapshotFidelity::PlainTextOnly`] snapshot returns `false` (causing guarded
    /// actions to fail closed with `unobservable_selection`), because plain-text reads
    /// lose reverse-video highlights—including unnumbered pure-highlight menus.
    pub const fn selection_observable(&self) -> bool {
        matches!(self.fidelity, SnapshotFidelity::StyledVt { .. })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TuiSnapshotWire {
    address: String,
    cols: u16,
    rows: u16,
    live_state: TerminalLiveState,
    title: Option<String>,
    fidelity: SnapshotFidelity,
    plain_lines: Vec<String>,
    agent: Option<String>,
    agent_status: AgentStatus,
    rule: Option<String>,
    blocker_visible: bool,
    choices: Vec<TuiChoice>,
    guard: ScreenGuard,
    snapshot_hash: u64,
}

impl Serialize for TuiSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = TuiSnapshotWire {
            address: self.address.clone(),
            cols: self.cols,
            rows: self.rows,
            live_state: self.live_state.clone(),
            title: self.title.clone(),
            fidelity: self.fidelity.clone(),
            plain_lines: self.plain_lines.clone(),
            agent: self.agent.clone(),
            agent_status: self.agent_status,
            rule: self.rule.clone(),
            blocker_visible: self.blocker_visible,
            choices: self.choices.clone(),
            guard: self.guard(),
            snapshot_hash: self.snapshot_hash,
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TuiSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = TuiSnapshotWire::deserialize(deserializer)?;
        let recomputed = hash_snapshot_parts(
            &wire.address,
            wire.cols,
            wire.rows,
            &wire.live_state,
            &wire.fidelity,
            &wire.plain_lines,
            &wire.choices,
        );
        if wire.snapshot_hash != recomputed || wire.guard.as_u64() != recomputed {
            return Err(DeError::custom(format!(
                "invalid TuiSnapshot: guard/snapshot_hash ({} / {:#018x}) does not match computed {recomputed:#018x}",
                wire.guard, wire.snapshot_hash
            )));
        }
        Ok(Self {
            address: wire.address,
            cols: wire.cols,
            rows: wire.rows,
            live_state: wire.live_state,
            title: wire.title,
            fidelity: wire.fidelity,
            plain_lines: wire.plain_lines,
            agent: wire.agent,
            agent_status: wire.agent_status,
            rule: wire.rule,
            blocker_visible: wire.blocker_visible,
            choices: wire.choices,
            snapshot_hash: wire.snapshot_hash,
        })
    }
}

fn clean_plain_line(raw: &str) -> String {
    strip_ansi(raw)
        .chars()
        .filter(|ch| !ch.is_control())
        .collect()
}

fn parse_herdr_first_paint(raw_vt: &str) -> (Vec<String>, Option<CursorPos>, bool) {
    let paint_end = ["\x1b[?9001h", "\x1b[?9001l"]
        .into_iter()
        .filter_map(|marker| raw_vt.find(marker))
        .min()
        .unwrap_or(raw_vt.len());
    let prefix = &raw_vt[..paint_end];
    let (body, alt_screen) = extract_screen_body(prefix);
    let cursor = extract_last_cup(body);
    let mut styled_lines: Vec<String> = body.lines().map(canonicalize_styled_line).collect();
    while styled_lines
        .last()
        .is_some_and(|line| clean_plain_line(line).is_empty() && !line_has_non_reset_sgr(line))
    {
        styled_lines.pop();
    }
    (styled_lines, cursor, alt_screen)
}

fn extract_screen_body(prefix: &str) -> (&str, bool) {
    for alt_marker in ["\x1b[?1049h\x1b[!p", "\x1b[?1049h"] {
        if let Some(pos) = prefix.rfind(alt_marker) {
            return (&prefix[pos + alt_marker.len()..], true);
        }
    }
    if let Some(pos) = prefix.rfind("\x1b[H\x1b[2J") {
        return (&prefix[pos + 7..], false);
    }
    (prefix, false)
}

fn extract_last_cup(body: &str) -> Option<CursorPos> {
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut last: Option<CursorPos> = None;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                end += 1;
            }
            if end < bytes.len()
                && bytes[end] == b'H'
                && let Some((row_s, col_s)) = body[start..end].split_once(';')
                && !row_s.is_empty()
                && !col_s.is_empty()
                && let (Ok(r), Ok(c)) = (row_s.parse::<u16>(), col_s.parse::<u16>())
            {
                last = Some(CursorPos {
                    row: r.saturating_sub(1),
                    col: c.saturating_sub(1),
                });
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    last
}

fn canonicalize_styled_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            if character != '\r' && !character.is_control() {
                out.push(character);
            }
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut final_ch = None;
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        final_ch = Some(next);
                        break;
                    }
                    params.push(next);
                }
                if final_ch == Some('m') {
                    out.push('\u{1b}');
                    out.push('[');
                    out.push_str(&params);
                    out.push('m');
                }
            }
            Some(']') => consume_osc_tail(&mut chars),
            Some(ch) if ('\u{20}'..='\u{2f}').contains(&ch) => consume_nf_tail(&mut chars),
            Some(ch) if ('\u{30}'..='\u{7e}').contains(&ch) => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

fn consume_osc_tail(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    chars.next();
    while let Some(next) = chars.next() {
        if next == '\u{7}' {
            break;
        }
        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

fn consume_nf_tail(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    chars.next();
    while chars
        .peek()
        .is_some_and(|next| ('\u{20}'..='\u{2f}').contains(next))
    {
        chars.next();
    }
    if chars
        .peek()
        .is_some_and(|next| ('\u{30}'..='\u{7e}').contains(next))
    {
        chars.next();
    }
}

fn line_has_non_reset_sgr(line: &str) -> bool {
    let mut idx = 0usize;
    while let Some(rel) = line[idx..].find("\x1b[") {
        let start = idx + rel + 2;
        if let Some(end_rel) = line[start..].find('m') {
            let params = &line[start..start + end_rel];
            if !params.is_empty() && params != "0" {
                return true;
            }
            idx = start + end_rel + 1;
        } else {
            break;
        }
    }
    false
}

/// Extracts interactive menu / prompt choices from raw VT or plain lines.
///
/// Tracks stateful SGR reverse-video (`\x1b[7m` without confusing palette index `7` in
/// `38;5;7` / `38;2;7;0;0`) alongside dedicated cursor markers (`❯`, `▸`, `▶`, `●`, `[*]`).
/// Plain `>` matches only when immediately followed by a numbered choice (`> 1. ...`) so
/// ordinary output like `> cargo build` or `> done` is never misclassified.
pub fn parse_tui_choices(raw_text: &str) -> Vec<TuiChoice> {
    let lines: Vec<String> = raw_text.lines().map(str::to_string).collect();
    parse_tui_choices_from_lines(&lines)
}

fn parse_tui_choices_from_lines(lines: &[String]) -> Vec<TuiChoice> {
    let mut choices = Vec::new();
    let mut inverse = false;
    for (line_index, raw_line) in lines.iter().enumerate() {
        let has_reverse_video = line_has_reverse_video_text(raw_line, &mut inverse);
        let plain = clean_plain_line(raw_line);
        let trimmed = plain.trim();
        if trimmed.is_empty() {
            continue;
        }
        let prefix = strip_selection_marker(trimmed);
        if let Some((index, rest)) = parse_numbered_choice(prefix.rest) {
            if !rest.is_empty() {
                choices.push(TuiChoice {
                    index: Some(index),
                    label: rest.to_string(),
                    selected: prefix.dedicated_glyph || prefix.gt_marker || has_reverse_video,
                    line_index,
                });
            }
        } else if prefix.dedicated_glyph && !prefix.rest.is_empty() {
            choices.push(TuiChoice {
                index: None,
                label: prefix.rest.to_string(),
                selected: true,
                line_index,
            });
        }
    }
    choices
}

fn line_has_reverse_video_text(raw_line: &str, inverse: &mut bool) -> bool {
    let mut saw_inverse_text = false;
    let mut chars = raw_line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_ch = None;
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            final_ch = Some(next);
                            break;
                        }
                        params.push(next);
                    }
                    if final_ch == Some('m') {
                        sgr_updates_inverse(&params, inverse);
                    }
                }
                Some(']') => consume_osc_tail(&mut chars),
                Some(n) if ('\u{20}'..='\u{2f}').contains(&n) => consume_nf_tail(&mut chars),
                Some(n) if ('\u{30}'..='\u{7e}').contains(&n) => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        if *inverse && !ch.is_whitespace() && !ch.is_control() {
            saw_inverse_text = true;
        }
    }
    saw_inverse_text
}

fn sgr_updates_inverse(params: &str, inverse: &mut bool) {
    if params.is_empty() {
        *inverse = false;
        return;
    }
    let mut iter = params.split(';');
    while let Some(code) = iter.next() {
        match code {
            "" | "0" | "27" => *inverse = false,
            "7" => *inverse = true,
            "38" | "48" | "58" => match iter.next() {
                Some("5") => {
                    let _ = iter.next();
                }
                Some("2") => {
                    let _ = (iter.next(), iter.next(), iter.next());
                }
                _ => {}
            },
            _ => {}
        }
    }
}

struct SelectionPrefix<'a> {
    dedicated_glyph: bool,
    gt_marker: bool,
    rest: &'a str,
}

fn strip_selection_marker(trimmed: &str) -> SelectionPrefix<'_> {
    for marker in ["❯", "▸", "▶", "●", "[*]"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return SelectionPrefix {
                dedicated_glyph: true,
                gt_marker: false,
                rest: rest.trim_start(),
            };
        }
    }
    if let Some(rest) = trimmed.strip_prefix('>') {
        return SelectionPrefix {
            dedicated_glyph: false,
            gt_marker: true,
            rest: rest.trim_start(),
        };
    }
    SelectionPrefix {
        dedicated_glyph: false,
        gt_marker: false,
        rest: trimmed,
    }
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

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn hash_u64(hash: &mut u64, value: u64) {
    hash_bytes(hash, &value.to_le_bytes());
}

fn hash_str(hash: &mut u64, value: &str) {
    hash_u64(hash, u64::try_from(value.len()).unwrap_or(u64::MAX));
    hash_bytes(hash, value.as_bytes());
}

fn hash_opt_str(hash: &mut u64, value: Option<&str>) {
    match value {
        Some(text) => {
            hash_bytes(hash, &[1]);
            hash_str(hash, text);
        }
        None => hash_bytes(hash, &[0]),
    }
}

fn hash_live_state(hash: &mut u64, live_state: &TerminalLiveState) {
    match live_state {
        TerminalLiveState::Live { pid } => {
            hash_bytes(hash, &[1]);
            match pid {
                Some(p) => {
                    hash_bytes(hash, &[1]);
                    hash_u64(hash, u64::from(*p));
                }
                None => hash_bytes(hash, &[0]),
            }
        }
        TerminalLiveState::Exited { cause } => {
            hash_bytes(hash, &[0]);
            match cause {
                ExitCause::Code { code } => {
                    hash_bytes(hash, &[1]);
                    hash_bytes(hash, &code.to_le_bytes());
                }
                ExitCause::Signal { signal } => {
                    hash_bytes(hash, &[2]);
                    hash_bytes(hash, &signal.to_le_bytes());
                }
                ExitCause::Lost { reason } => {
                    hash_bytes(hash, &[3]);
                    hash_opt_str(hash, reason.as_deref());
                }
            }
        }
    }
}

fn hash_read_mode(hash: &mut u64, mode: &ReadMode) {
    match mode {
        ReadMode::Screen => hash_bytes(hash, &[1]),
        ReadMode::History => hash_bytes(hash, &[2]),
        ReadMode::Stitched => hash_bytes(hash, &[3]),
        ReadMode::Other(raw) => {
            hash_bytes(hash, &[4]);
            hash_str(hash, raw);
        }
    }
}

fn hash_snapshot_parts(
    address: &str,
    cols: u16,
    rows: u16,
    live_state: &TerminalLiveState,
    fidelity: &SnapshotFidelity,
    plain_lines: &[String],
    choices: &[TuiChoice],
) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    hash_str(&mut hash, address);
    hash_u64(&mut hash, u64::from(cols));
    hash_u64(&mut hash, u64::from(rows));
    hash_live_state(&mut hash, live_state);
    match fidelity {
        SnapshotFidelity::StyledVt {
            cursor,
            alt_screen,
            styled_lines,
        } => {
            hash_bytes(&mut hash, &[1, u8::from(*alt_screen)]);
            match cursor {
                Some(pos) => {
                    hash_bytes(&mut hash, &[1]);
                    hash_u64(&mut hash, u64::from(pos.row));
                    hash_u64(&mut hash, u64::from(pos.col));
                }
                None => hash_bytes(&mut hash, &[0]),
            }
            hash_u64(
                &mut hash,
                u64::try_from(styled_lines.len()).unwrap_or(u64::MAX),
            );
            for line in styled_lines {
                hash_str(&mut hash, line);
            }
        }
        SnapshotFidelity::PlainTextOnly {
            read_mode,
            complete,
            why,
        } => {
            hash_bytes(&mut hash, &[0, u8::from(*complete)]);
            hash_read_mode(&mut hash, read_mode);
            hash_opt_str(&mut hash, why.as_deref());
        }
    }
    hash_u64(
        &mut hash,
        u64::try_from(plain_lines.len()).unwrap_or(u64::MAX),
    );
    for line in plain_lines {
        hash_str(&mut hash, line);
    }
    hash_u64(&mut hash, u64::try_from(choices.len()).unwrap_or(u64::MAX));
    for choice in choices {
        match choice.index {
            Some(idx) => hash_bytes(&mut hash, &[1, idx]),
            None => hash_bytes(&mut hash, &[0]),
        }
        hash_str(&mut hash, &choice.label);
        hash_bytes(&mut hash, &[u8::from(choice.selected)]);
        hash_u64(
            &mut hash,
            u64::try_from(choice.line_index).unwrap_or(u64::MAX),
        );
    }
    hash
}

/// Optional status precondition paired with [`SendPolicy::OnlyIf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusPrecondition {
    /// Require the agent status to equal `"idle"` (`if = "idle"`).
    Idle,
    /// Require the agent status to equal `"working"` (`if = "working"`).
    Working,
}

/// Non-contradictory delivery precondition passed to `terminal.send`.
///
/// [`Self::ForceEvenIfBlocked`] and [`Self::OnlyIf`] are mutually exclusive
/// variants so `--force` and `--if-guard` / status preconditions are
/// unrepresentable together at both type and wire boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SendPolicy {
    /// Send unless the agent status is currently `blocked`.
    #[default]
    WhenUnblocked,
    /// Deliver even when the agent reports `blocked` (`force = true`).
    ForceEvenIfBlocked,
    /// Require an optimistic pre-flight [`ScreenGuard`] (and optional status precondition).
    OnlyIf {
        /// Optimistic pre-flight screen digest required before sending.
        expected_guard: ScreenGuard,
        /// Optional status precondition (`"idle"` or `"working"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<StatusPrecondition>,
    },
    /// Require the agent status to equal `"idle"` (`if = "idle"`, `force = false`).
    OnlyIfIdle,
    /// Require the agent status to equal `"working"` (`if = "working"`, `force = false`).
    OnlyIfWorking,
}

impl<'de> Deserialize<'de> for SendPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Kind {
            WhenUnblocked,
            ForceEvenIfBlocked,
            OnlyIf,
            OnlyIfIdle,
            OnlyIfWorking,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: Kind,
            expected_guard: Option<ScreenGuard>,
            status: Option<StatusPrecondition>,
        }

        let wire = Wire::deserialize(deserializer)?;
        match (wire.kind, wire.expected_guard, wire.status) {
            (Kind::WhenUnblocked, None, None) => Ok(Self::WhenUnblocked),
            (Kind::ForceEvenIfBlocked, None, None) => Ok(Self::ForceEvenIfBlocked),
            (Kind::OnlyIf, Some(expected_guard), status) => Ok(Self::OnlyIf {
                expected_guard,
                status,
            }),
            (Kind::OnlyIf, None, _) => Err(DeError::missing_field("expected_guard")),
            (Kind::OnlyIfIdle, None, None) => Ok(Self::OnlyIfIdle),
            (Kind::OnlyIfWorking, None, None) => Ok(Self::OnlyIfWorking),
            (_, Some(_), _) => Err(DeError::custom(
                "expected_guard is only valid with send policy kind=\"only_if\"",
            )),
            (_, None, Some(_)) => Err(DeError::custom(
                "status is only valid with send policy kind=\"only_if\"",
            )),
        }
    }
}

impl SendPolicy {
    /// Optimistic pre-flight guard required by this policy (`Some` only for [`Self::OnlyIf`]).
    pub const fn expected_guard(self) -> Option<ScreenGuard> {
        match self {
            Self::OnlyIf { expected_guard, .. } => Some(expected_guard),
            Self::WhenUnblocked
            | Self::ForceEvenIfBlocked
            | Self::OnlyIfIdle
            | Self::OnlyIfWorking => None,
        }
    }

    /// Returns `(force, if_param)` wire parameters for `herdr-core` `terminal.send`.
    pub const fn to_herdr_params(self) -> (bool, Option<&'static str>) {
        match self {
            Self::WhenUnblocked | Self::OnlyIf { status: None, .. } => (false, None),
            Self::ForceEvenIfBlocked => (true, None),
            Self::OnlyIf {
                status: Some(StatusPrecondition::Idle),
                ..
            }
            | Self::OnlyIfIdle => (false, Some("idle")),
            Self::OnlyIf {
                status: Some(StatusPrecondition::Working),
                ..
            }
            | Self::OnlyIfWorking => (false, Some("working")),
        }
    }
}

/// Declarative action executed against a live terminal via `HerdrGlobalClient::execute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TuiAction {
    /// Submit pasted text + Enter, governed strictly by [`SendPolicy`].
    SendText {
        /// Text to paste and submit.
        text: String,
        /// Delivery policy (excludes contradictory `force=true` + `expected_guard`/`only_if`).
        #[serde(default)]
        policy: SendPolicy,
        /// Optional wait timeout in milliseconds once text is sent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_timeout_ms: Option<u64>,
    },
    /// Send a non-empty sequence of VT key chords in a single `terminal.keys` RPC.
    SendKeys {
        /// Non-empty validated sequence of VT key chords.
        keys: KeyChordSequence,
        /// Optional optimistic pre-flight guard token (checked before sending).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_guard: Option<ScreenGuard>,
    },
    /// Interrupt the foreground program (`terminal.interrupt`).
    Interrupt {
        /// Optional observation timeout in milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Event-driven wait until the terminal leaves `working` or emits matching output.
    WaitSettled {
        /// Optional previously observed turn counter for fast-path skip.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_turn: Option<u64>,
        /// Optional plain-text substring trigger in subsequent terminal output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_contains: Option<String>,
        /// Wait timeout in milliseconds.
        timeout_ms: u64,
    },
}

/// Typed outcome returned by `HerdrGlobalClient::execute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome_kind", rename_all = "snake_case")]
pub enum TuiActionOutcome {
    /// Text was submitted through `terminal.send`.
    TextSent {
        /// Current OSC 7501 status word if reported.
        status: Option<String>,
        /// Terminal turn counter after send.
        turn: u64,
        /// Whether `terminal.send` reported that a turn started.
        started: Option<bool>,
    },
    /// Key chords were delivered through a single `terminal.keys` RPC.
    KeysSent {
        /// Current OSC 7501 status word if reported.
        status: Option<String>,
        /// Terminal turn counter after keys delivery.
        turn: u64,
    },
    /// Foreground program interrupt report.
    Interrupted(InterruptReport),
    /// Event-driven wait result.
    Waited(WaitOutcome),
}

/// Result classification returned by `herdr-core` `terminal.interrupt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "raw", rename_all = "snake_case")]
pub enum InterruptOutcome {
    /// Program left `working` or returned to its prompt.
    Stopped,
    /// Process exited as a result of interrupting.
    Exited,
    /// Program remained `working` past the observation window.
    StillWorking,
    /// Program explicitly does not report status (`"unknown"` on wire).
    Unreported,
    /// Unrecognized future `outcome` value preserved verbatim.
    Other(String),
}

impl InterruptOutcome {
    /// Parses Herdr's `InterruptResult::outcome` wire string.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "stopped" => Self::Stopped,
            "exited" => Self::Exited,
            "still_working" => Self::StillWorking,
            "unknown" => Self::Unreported,
            other => Self::Other(other.to_string()),
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

/// Closed cause explaining why [`WaitOutcome`] completed, carrying variant-only payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitReason {
    /// Status transitioned out of `working` (for example `idle`, `done`, `blocked`, `error`).
    Status,
    /// Terminal process exited.
    Exit {
        /// Mutually exclusive exit cause.
        cause: ExitCause,
    },
    /// Output produced during the wait matched the requested substring.
    Output,
    /// Timeout expired before settling (`ended() == false`).
    Timeout,
    /// Terminal never transitioned to `working` after a tracked send (`ended() == false`).
    NotStarted,
    /// Child-wait aggregation returned (`--children`).
    Children,
    /// Wait woke early because another caller messaged the terminal (`ended() == false`).
    Message {
        /// Sender address when reported.
        from: Option<String>,
    },
    /// Unrecognized future `reason` value preserved verbatim with its wire `ended` flag.
    Other {
        /// Raw `reason` string from Herdr.
        raw: String,
        /// Whether Herdr reported the wait as settled (`ended`).
        ended: bool,
    },
}

impl WaitReason {
    /// Whether this reason represents a settled completion (`true`) or an unsettled wakeup (`false`).
    pub const fn ended(&self) -> bool {
        match self {
            Self::Status | Self::Exit { .. } | Self::Output | Self::Children => true,
            Self::Timeout | Self::NotStarted | Self::Message { .. } => false,
            Self::Other { ended, .. } => *ended,
        }
    }

    /// Parses Herdr's wire `WaitResult` fields into a non-contradictory [`WaitReason`].
    pub fn from_herdr(
        raw: &str,
        wire_ended: bool,
        exit_code: Option<i32>,
        signal: Option<i32>,
        from: Option<String>,
    ) -> Result<Self, String> {
        let parsed = match raw {
            "status" => Self::Status,
            "exit" => Self::Exit {
                cause: ExitCause::from_herdr(exit_code, signal, None)?,
            },
            "output" => Self::Output,
            "timeout" => Self::Timeout,
            "not_started" => Self::NotStarted,
            "children" => Self::Children,
            "message" => Self::Message { from },
            other => Self::Other {
                raw: other.to_string(),
                ended: wire_ended,
            },
        };
        if !matches!(parsed, Self::Other { .. }) && parsed.ended() != wire_ended {
            return Err(format!(
                "contradictory Herdr wait result: reason={raw:?} implies ended={}, got ended={wire_ended}",
                parsed.ended()
            ));
        }
        Ok(parsed)
    }
}

/// Typed result of an event-driven `terminal.wait` call with `ended()` derived from `reason`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitOutcome {
    /// Terminal address reported by Herdr.
    pub terminal: String,
    /// Structured wait completion cause (carrying any exit cause or message sender).
    pub reason: WaitReason,
    /// Current OSC 7501 status word if reported.
    pub status: Option<String>,
    /// Terminal turn counter when the wait returned.
    pub turn: u64,
}

impl WaitOutcome {
    /// True if settled; false on `Timeout`, `Message`, or `NotStarted`.
    pub const fn ended(&self) -> bool {
        self.reason.ended()
    }
}

// `port.rs` is left untouched to remain disjoint from open Wave-1 PRs; `AgentStatus`
// serde lives here within the same crate (`spawn-core`).
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
    fn key_chord_parses_roundtrips_and_enforces_closed_domain() {
        for (input, expected_spec) in [
            ("ctrl+c", "ctrl+c"),
            ("Control+Shift+Tab", "ctrl+shift+tab"),
            ("alt+shift+tab", "alt+shift+tab"),
            ("digit0", "0"),
            ("digit3", "3"),
            ("F12", "f12"),
            ("backslash", "\\"),
        ] {
            let chord = KeyChord::parse(input).expect("valid chord");
            assert_eq!(chord.to_herdr_spec(), expected_spec);
            let encoded = serde_json::to_string(&chord).expect("serialize");
            let decoded: KeyChord = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(decoded, chord);
        }

        for key in NamedKey::ALL {
            let chord = KeyChord::from(key);
            let reparsed = KeyChord::parse(&chord.to_herdr_spec()).expect("named key reparses");
            assert_eq!(reparsed, chord);
        }

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
            "f01",
            "f13",
            "digitx",
            "hello",
        ] {
            assert!(KeyChord::parse(bad).is_err(), "must reject {bad:?}");
        }
        assert!(KeyChar::new('+').is_err());
        assert!(KeyChar::new('A').is_err());
        assert!(KeyChordSequence::new(Vec::new()).is_err());
        assert!(serde_json::from_str::<KeyChordSequence>("[]").is_err());
    }

    #[test]
    fn clean_plain_line_handles_multibyte_after_esc_and_ignores_ordinary_gt_output() {
        assert_eq!(clean_plain_line("x\x1b❯ 1. Allow"), "x❯ 1. Allow");
        assert_eq!(clean_plain_line("\x1b(Bhello\x1b(0world"), "helloworld");

        let plain_prose = "> cargo build\n> done\n\n";
        assert!(parse_tui_choices(plain_prose).is_empty());

        let fg_palette_seven = "\x1b[38;5;7m1. First\x1b[0m\n2. Second\n";
        let choices = parse_tui_choices(fg_palette_seven);
        assert_eq!(choices.len(), 2);
        assert!(!choices[0].selected);
        assert!(!choices[1].selected);
    }

    #[test]
    fn styled_vt_detects_highlight_shift_without_plain_text_change_and_checks_integrity() {
        let screen_sel1 = concat!(
            "\x18\x1b[!p\x1b[?1049l\x1b[H\x1b[2J",
            "Select action:\r\n",
            "  \x1b[0;7m1. Allow action\x1b[0m\r\n",
            "  2. Deny action",
            "\x1b(B\x1b[0m\x1b[2;3H\x1b[?25l\x1b[?9001l",
        );
        let screen_sel2 = concat!(
            "\x18\x1b[!p\x1b[?1049l\x1b[H\x1b[2J",
            "Select action:\r\n",
            "  1. Allow action\r\n",
            "  \x1b[0;7m2. Deny action\x1b[0m",
            "\x1b(B\x1b[0m\x1b[3;3H\x1b[?25l\x1b[?9001l",
        );
        let live = TerminalLiveState::Live { pid: Some(123) };
        let snap1 = TuiSnapshot::from_styled_vt(
            RawStyledSnapshot {
                address: "t1",
                cols: 120,
                rows: 40,
                live_state: live.clone(),
                title: Some("menu"),
                raw_vt: screen_sel1,
            },
            SnapshotObservation::default(),
        );
        let snap2 = TuiSnapshot::from_styled_vt(
            RawStyledSnapshot {
                address: "t1",
                cols: 120,
                rows: 40,
                live_state: live.clone(),
                title: Some("menu"),
                raw_vt: screen_sel2,
            },
            SnapshotObservation::default(),
        );
        assert_eq!(snap1.plain_lines(), snap2.plain_lines());
        assert_eq!(snap1.live_state(), &live);
        assert_eq!(snap1.read_mode(), ReadMode::Screen);
        assert!(snap1.complete());
        assert!(snap1.why().is_none());
        assert!(snap1.styled_lines().is_some());
        assert!(matches!(
            snap1.fidelity(),
            SnapshotFidelity::StyledVt { styled_lines, .. } if !styled_lines.is_empty()
        ));
        assert!(snap1.choices()[0].selected && !snap1.choices()[1].selected);
        assert!(!snap2.choices()[0].selected && snap2.choices()[1].selected);
        assert_ne!(snap1.guard(), snap2.guard());
        assert!(snap1.selection_observable());

        let base_json: serde_json::Value =
            serde_json::to_value(&snap1).expect("serialize snapshot");
        assert_eq!(
            base_json["guard"].as_str(),
            Some(snap1.guard().to_string().as_str())
        );
        let parsed: TuiSnapshot =
            serde_json::from_value(base_json.clone()).expect("deserialize valid snapshot");
        assert_eq!(parsed, snap1);

        let mut bad_hash = base_json.clone();
        bad_hash["snapshot_hash"] = serde_json::json!(snap1.snapshot_hash() ^ 1);
        assert!(serde_json::from_value::<TuiSnapshot>(bad_hash).is_err());

        let mut bad_guard = base_json.clone();
        bad_guard["guard"] = serde_json::json!(ScreenGuard::from_raw(snap1.snapshot_hash() ^ 2));
        assert!(serde_json::from_value::<TuiSnapshot>(bad_guard).is_err());

        let mut tampered_styled = base_json;
        tampered_styled["fidelity"]["styled_lines"][0] = serde_json::json!("Tampered");
        assert!(serde_json::from_value::<TuiSnapshot>(tampered_styled).is_err());
    }

    #[test]
    fn plain_text_only_fidelity_is_never_selection_observable() {
        let live = TerminalLiveState::Live { pid: Some(123) };
        let plain_unmarked = TuiSnapshot::from_plain_text(RawPlainSnapshot {
            address: "t1",
            cols: 120,
            rows: 40,
            live_state: live.clone(),
            how: "screen",
            complete: true,
            why: None,
            title: None,
            raw_text: "Select action:\n  1. Allow action\n  2. Deny action\n",
            agent: None,
            agent_status: AgentStatus::Unknown,
            rule: None,
            blocker_visible: false,
        });
        assert!(!plain_unmarked.selection_observable());
        assert!(plain_unmarked.styled_lines().is_none());
        assert!(matches!(
            plain_unmarked.fidelity(),
            SnapshotFidelity::PlainTextOnly {
                read_mode: ReadMode::Screen,
                complete: true,
                why: None,
            }
        ));

        // Even unnumbered pure-highlight menus or plain glyph-marked screens MUST return
        // `selection_observable() == false` under PlainTextOnly (fail-closed).
        for raw_text in [
            "Choose provider:\n  Claude\n  Codex\n",
            "Select action:\n  ❯ 1. Allow action\n  2. Deny action\n",
        ] {
            let plain = TuiSnapshot::from_plain_text(RawPlainSnapshot {
                address: "t1",
                cols: 120,
                rows: 40,
                live_state: live.clone(),
                how: "screen",
                complete: true,
                why: None,
                title: None,
                raw_text,
                agent: None,
                agent_status: AgentStatus::Unknown,
                rule: None,
                blocker_visible: false,
            });
            assert!(!plain.selection_observable(), "{raw_text:?}");
        }
    }

    #[test]
    fn closed_exit_wait_interrupt_and_reserved_labels_enforced() {
        assert!(ExitCause::from_herdr(Some(0), Some(9), None).is_err());
        assert_eq!(
            ExitCause::from_herdr(None, Some(15), Some("signaled".to_string())).expect("signal"),
            ExitCause::Signal { signal: 15 }
        );

        let timeout_reason =
            WaitReason::from_herdr("timeout", false, None, None, None).expect("timeout");
        assert!(!timeout_reason.ended());
        assert!(WaitReason::from_herdr("timeout", true, None, None, None).is_err());
        assert!(WaitReason::from_herdr("exit", true, Some(0), Some(9), None).is_err());

        let not_started =
            WaitReason::from_herdr("not_started", false, None, None, None).expect("not_started");
        assert!(!not_started.ended());

        let msg = WaitReason::from_herdr("message", false, None, None, Some("t9".to_string()))
            .expect("message");
        assert!(!msg.ended());

        assert_eq!(
            InterruptOutcome::parse("unknown"),
            InterruptOutcome::Unreported
        );
        assert_eq!(
            InterruptOutcome::parse("escalated"),
            InterruptOutcome::Other("escalated".to_string())
        );

        assert!(is_reserved_spawn_label(SPAWN_OWNER_LABEL));
        assert!(is_reserved_spawn_label(SPAWN_TERMINAL_LABEL));
        assert!(is_reserved_spawn_label(SPAWN_WORKSPACE_LABEL));
        assert!(is_reserved_spawn_label("SPAWN_CUSTOM"));
        assert!(is_reserved_spawn_label(" spawn_extra"));
        assert!(!is_reserved_spawn_label("observed_by"));
        assert!(!is_reserved_spawn_label("a❯❯"));

        let g = ScreenGuard::from_raw(0x0123_4567_89ab_cdef);
        assert_eq!(ScreenGuard::parse(&g.to_string()), Ok(g));
        assert_eq!(ScreenGuard::parse("42"), Ok(ScreenGuard::from_raw(42)));
        assert_eq!(
            serde_json::from_str::<ScreenGuard>(&format!("\"{g}\"")).expect("parse hex guard"),
            g
        );
        assert_eq!(
            serde_json::from_str::<ScreenGuard>("42").expect("parse numeric guard"),
            ScreenGuard::from_raw(42)
        );
        for bad in ["", " 0x1", "0x1 ", "0x", "0x1234567890abcdef0", "not-hex"] {
            assert!(ScreenGuard::parse(bad).is_err(), "{bad:?}");
        }

        let act = TuiAction::SendKeys {
            keys: KeyChordSequence::single(KeyChord::parse("enter").expect("enter")),
            expected_guard: Some(ScreenGuard::from_raw(42)),
        };
        let encoded = serde_json::to_string(&act).expect("serialize action");
        let decoded: TuiAction = serde_json::from_str(&encoded).expect("deserialize action");
        assert_eq!(decoded, act);

        let send_guarded = TuiAction::SendText {
            text: "hi".to_string(),
            policy: SendPolicy::OnlyIf {
                expected_guard: g,
                status: Some(StatusPrecondition::Idle),
            },
            wait_timeout_ms: Some(1500),
        };
        assert_eq!(SendPolicy::WhenUnblocked.expected_guard(), None);
        assert_eq!(SendPolicy::ForceEvenIfBlocked.expected_guard(), None);
        let send_json = serde_json::to_string(&send_guarded).expect("serialize send");
        assert_eq!(
            serde_json::from_str::<TuiAction>(&send_json).expect("roundtrip send"),
            send_guarded
        );
        assert!(
            serde_json::from_str::<TuiAction>(
                r#"{"action":"send_text","text":"hi","policy":{"kind":"force_even_if_blocked"},"expected_guard":"0x1"}"#
            )
            .is_err(),
            "SendText must reject legacy top-level expected_guard"
        );
        assert!(
            serde_json::from_str::<TuiAction>(
                r#"{"action":"send_text","text":"hi","policy":{"kind":"force_even_if_blocked","expected_guard":"0x1"}}"#
            )
            .is_err(),
            "ForceEvenIfBlocked must reject expected_guard"
        );
    }

    #[test]
    fn terminal_address_validates_local_and_federated_and_rejects_traversal_and_junk() {
        let local = TerminalAddress::parse("t1").expect("local addr");
        assert_eq!(local.as_str(), "t1");
        assert_eq!(local.machine(), None);
        assert_eq!(local.terminal(), "t1");

        let remote = TerminalAddress::parse("n252/t3").expect("remote addr");
        assert_eq!(remote.as_str(), "n252/t3");
        assert_eq!(remote.machine(), Some("n252"));
        assert_eq!(remote.terminal(), "t3");
        let rt: TerminalAddress =
            serde_json::from_str(&serde_json::to_string(&remote).expect("ser")).expect("roundtrip");
        assert_eq!(rt, remote);

        for bad in [
            "", " ", "t 1", "/t1", "t1/", "a//b", "a/b/c", ".", "..", "n252/..", "../t1", "t1\n",
        ] {
            assert!(TerminalAddress::parse(bad).is_err(), "parse {bad:?}");
            assert!(
                serde_json::from_str::<TerminalAddress>(&serde_json::json!(bad).to_string())
                    .is_err(),
                "serde {bad:?}"
            );
        }
    }

    #[test]
    fn nested_snapshot_wire_types_reject_unknown_fields() {
        let styled_snap = TuiSnapshot::from_styled_vt(
            RawStyledSnapshot {
                address: "t1",
                cols: 120,
                rows: 40,
                live_state: TerminalLiveState::Live { pid: Some(7) },
                title: None,
                raw_vt: "\x1b[H\x1b[2J  \x1b[0;7m1. Allow\x1b[0m\r\n\x1b[2;3H\x1b[?9001l",
            },
            SnapshotObservation::default(),
        );
        let base = serde_json::to_value(&styled_snap).expect("serialize styled");

        for mutate in [
            |v: &mut serde_json::Value| {
                v["fidelity"]["bogus"] = serde_json::json!(1);
            },
            |v: &mut serde_json::Value| {
                v["fidelity"]["cursor"]["bogus"] = serde_json::json!(1);
            },
            |v: &mut serde_json::Value| {
                v["choices"][0]["bogus"] = serde_json::json!(1);
            },
            |v: &mut serde_json::Value| {
                v["live_state"]["bogus"] = serde_json::json!(1);
            },
        ] {
            let mut copy = base.clone();
            mutate(&mut copy);
            assert!(
                serde_json::from_value::<TuiSnapshot>(copy.clone()).is_err(),
                "styled must reject unknown field: {copy}"
            );
        }

        let exited_plain = TuiSnapshot::from_plain_text(RawPlainSnapshot {
            address: "t2",
            cols: 80,
            rows: 24,
            live_state: TerminalLiveState::Exited {
                cause: ExitCause::Code { code: 0 },
            },
            how: "screen",
            complete: true,
            why: None,
            title: None,
            raw_text: "done\n",
            agent: None,
            agent_status: AgentStatus::Unknown,
            rule: None,
            blocker_visible: false,
        });
        let plain_base = serde_json::to_value(&exited_plain).expect("serialize exited plain");
        for mutate in [
            |v: &mut serde_json::Value| {
                v["fidelity"]["read_mode"]["bogus"] = serde_json::json!(1);
            },
            |v: &mut serde_json::Value| {
                v["fidelity"]["read_mode"]["detail"] = serde_json::json!("x");
            },
            |v: &mut serde_json::Value| {
                v["live_state"]["cause"]["bogus"] = serde_json::json!(1);
            },
        ] {
            let mut copy = plain_base.clone();
            mutate(&mut copy);
            assert!(
                serde_json::from_value::<TuiSnapshot>(copy.clone()).is_err(),
                "plain must reject unknown field: {copy}"
            );
        }
    }
}
