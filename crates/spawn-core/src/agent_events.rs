//! Agent Runtime multi-source event fusion, content-based session schema
//! sniffing, and multi-format JSONL parsing (`Wave-AR AR-4A`).
//!
//! Pure domain layer (no IO, no clocks, `#![forbid(unsafe_code)]`):
//! - Closed sum/product ADTs for agent engines, session formats, discovery
//!   provenance, event origins, fidelity, canonical deduplication keys, and
//!   fused event payloads;
//! - Content-based schema sniffer identifying Claude Code, Codex nested rollout
//!   JSONL, Antigravity CLI (`agy`) `transcript_full.jsonl`, OpenCode, and generic
//!   JSONL sessions while rejecting unrelated JSON/config files;
//! - Multi-format parsers with automatic credential redaction
//!   ([`crate::redact::redact`]) and bounded summaries (`<= 300` chars);
//! - Union + deduplication + phase correlation + progressive fidelity upgrade
//!   (`ScreenOnly` -> `SessionEnriched`) across `HerdrPty`, `NativeSession`, and
//!   `SpawnControl`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ingest::utc_ms;
use crate::redact::redact;

/// Maximum Unicode scalar values kept in any free-text summary field.
pub const MAX_SUMMARY_CHARS: usize = 300;

/// Maximum events retained by default when callers do not pass an explicit cap.
pub const DEFAULT_FUSED_EVENT_LIMIT: usize = 100;

/// Coding-agent engine identified in a host terminal or native session log.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "engine", content = "name", rename_all = "snake_case")]
pub enum AgentEngine {
    /// Anthropic Claude Code CLI (`claude`).
    Claude,
    /// Antigravity CLI (`agy`).
    Agy,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// OpenCode CLI (`opencode`).
    OpenCode,
    /// Google Gemini CLI (`gemini`).
    Gemini,
    /// Any other interactive coding CLI identified by executable basename.
    Generic(String),
}

impl AgentEngine {
    /// Canonical lowercase engine identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Generic(name) => name.as_str(),
        }
    }

    /// Match a known agent CLI binary basename (case-sensitive POSIX executable name).
    #[must_use]
    pub fn from_binary_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "agy" => Some(Self::Agy),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    /// Construct a [`Self::Generic`] variant if `name` is a clean safe token
    /// (`1..=64` ASCII alphanumerics, `-`, `_`, `.`).
    #[must_use]
    pub fn generic(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        if trimmed.is_empty()
            || trimmed.len() > 64
            || !trimmed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return None;
        }
        Some(Self::from_binary_name(trimmed).unwrap_or_else(|| Self::Generic(trimmed.to_string())))
    }
}

/// Format of an auto-discovered native agent session/transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFileFormat {
    /// Claude Code `$CLAUDE_CONFIG_DIR/projects/<ws>/<id>.jsonl`.
    ClaudeJsonl,
    /// Codex rollout JSONL (`YYYY/MM/DD/rollout-*.jsonl` or flat JSONL).
    CodexRolloutJsonl,
    /// Antigravity CLI (`agy`) `transcript_full.jsonl` or `transcript.jsonl`.
    AgyFullJsonl,
    /// OpenCode session JSONL log.
    OpenCodeJsonl,
    /// Generic structured agent JSONL log.
    GenericJsonl,
}

/// Mechanism that discovered the native session file for a live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDiscoveryMethod {
    /// Reflected directly from an open file descriptor under `/proc/<pid>/fd/*`.
    ProcessOpenFd,
    /// Matched dynamically from target process `cwd` and non-secret env roots.
    ProcessEnvAndCwd,
    /// Resolved from explicit caller/driver root hints.
    DriverHint,
}

/// Confidence level assigned to an auto-discovered [`SessionAttachment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryConfidence {
    /// Exact open FD held by a live process inside the terminal tree.
    ExactFd,
    /// Closed/closed-between-writes file verified via matching `cwd` or session ID.
    MatchedMetadata,
    /// Recent valid session file inside an agent root without `cwd` metadata.
    HeuristicRecent,
}

/// Metadata linking a discovered [`DiscoveredTerminal`](crate::tui::DiscoveredTerminal)
/// to its native agent session log on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachment {
    /// Canonical filesystem path of the discovered session log.
    pub path: String,
    /// Schema format verified by content sniffing.
    pub format: SessionFileFormat,
    /// Discovery mechanism that located the file.
    pub method: SessionDiscoveryMethod,
    /// Verification confidence of the match.
    pub confidence: DiscoveryConfidence,
    /// Native conversation/session identifier when extracted from file content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Working directory recorded inside the session metadata, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Source stream that contributed to a [`FusedAgentEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOrigin {
    /// Live PTY/VT/OSC 7501 and OS `/proc` witness observation.
    HerdrPty,
    /// Incremental native session/transcript JSONL record.
    NativeSession,
    /// Explicit Spawn control-plane or queue command record.
    SpawnControl,
}

/// Semantic fidelity level of a [`FusedAgentEvent`] or [`FusedEventsReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventFidelity {
    /// Derived solely from PTY screen/process observation (no session log enrichment).
    ScreenOnly,
    /// Derived solely from Spawn control/queue records.
    ControlOnly,
    /// Enriched by parsed native agent session JSONL (alone or fused with PTY/control).
    SessionEnriched,
}

/// Tool lifecycle phase used to pair deduplication keys cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    /// Invocation requested/started.
    Started,
    /// Invocation finished (success or failure).
    Completed,
}

/// Deterministic identity key used for union deduplication across event sources.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalEventKey {
    /// A tool call phase keyed by its tool-use/call identifier.
    ToolCall {
        /// Producer-assigned call ID (for example `toolu_...` or `call_...`).
        id: String,
        /// Start vs completion phase.
        phase: ToolPhase,
    },
    /// A native log record keyed by producer UUID or deterministic line key.
    NativeRecord {
        /// Unique identifier within the native session file.
        source_id: String,
    },
    /// A Spawn control command submission.
    ControlCommand {
        /// Command identifier or idempotency reference.
        id: String,
    },
    /// A screen-observed prompt/status transition keyed by frame digest + rule.
    ScreenSignal {
        /// Live screen/snapshot guard digest.
        digest: u64,
        /// Witness/screen rule identifier.
        rule: String,
    },
}

impl CanonicalEventKey {
    /// Stable string wire representation embedded into [`FusedAgentEvent::canonical_key`].
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::ToolCall { id, phase } => {
                let suffix = match phase {
                    ToolPhase::Started => "started",
                    ToolPhase::Completed => "completed",
                };
                format!("tool:{id}:{suffix}")
            }
            Self::NativeRecord { source_id } => format!("native:{source_id}"),
            Self::ControlCommand { id } => format!("ctrl:{id}"),
            Self::ScreenSignal { digest, rule } => format!("screen:{digest:016x}:{rule}"),
        }
    }
}

/// Normalized closed event payload produced by multi-source agent fusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FusedEventKind {
    /// Session header/context metadata (engine, CLI version, cwd, model).
    SessionMeta {
        /// Engine identifier if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        engine: Option<String>,
        /// Agent CLI version string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cli_version: Option<String>,
        /// Workspace directory reported by the session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Active model identifier if recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Model/agent execution turn started.
    TurnStarted {
        /// Turn identifier when provided by the runtime.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// Model/agent execution turn completed or terminated.
    TurnCompleted {
        /// Turn identifier when provided by the runtime.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Elapsed turn duration in milliseconds if reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// Redacted error message if the turn failed/aborted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Prompt or input submitted to the agent.
    UserInput {
        /// Bounded, redacted preview of the input text.
        summary: String,
    },
    /// Reasoning/thinking block emitted by the agent.
    Thinking {
        /// Bounded, redacted summary of thinking text.
        summary: String,
        /// Elapsed thinking time in milliseconds if recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Assistant textual response emitted by the agent.
    AssistantMessage {
        /// Bounded, redacted summary of assistant output.
        summary: String,
    },
    /// Tool call requested or begun.
    ToolStarted {
        /// Native tool call ID (`tool_use_id` / `call_id`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Tool name (`Bash`, `Read`, `Edit`, ...).
        tool_name: String,
        /// Bounded, redacted summary of the call arguments.
        summary: String,
    },
    /// Tool call completed (succeeded or failed).
    ToolCompleted {
        /// Native tool call ID (`tool_use_id` / `call_id`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Tool name if known from the completion record or correlated start.
        tool_name: String,
        /// Whether the invocation succeeded.
        ok: bool,
        /// Execution duration in milliseconds if recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// Bounded, redacted outcome/error summary.
        summary: String,
    },
    /// Token accounting checkpoint recorded for an API step/turn or cumulative session snapshot.
    UsageRecorded {
        /// Model name when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Prompt/input tokens billed.
        input_tokens: u64,
        /// Completion/output tokens billed.
        output_tokens: u64,
        /// Prompt cache read tokens.
        cache_read_tokens: u64,
        /// Prompt cache write tokens.
        cache_write_tokens: u64,
        /// True when this record reports a cumulative session-total snapshot
        /// (such as Claude `cost-state.modelUsage`) rather than a single step/message delta.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cumulative: bool,
    },
    /// Interactive UI prompt/blocker observed directly on the live terminal.
    PromptBlocked {
        /// Matching screen/witness blocker rule.
        rule: String,
        /// Number of parsed selectable choices on screen.
        choices_count: usize,
    },
    /// Coarse live process/terminal activity state observed from PTY/OSC/process tree.
    ProcessState {
        /// Activity label (`working`, `idle`, `blocked`, `exited`, ...).
        status: String,
        /// Optional matched rule name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
    },
    /// Error message reported in the session log or terminal stream.
    ErrorObserved {
        /// Bounded, redacted diagnostic text.
        message: String,
    },
}

impl FusedEventKind {
    const fn richness_score(&self) -> u8 {
        match self {
            Self::ProcessState { .. } => 1,
            Self::PromptBlocked { .. } => 2,
            Self::TurnStarted { .. }
            | Self::TurnCompleted { .. }
            | Self::SessionMeta { .. }
            | Self::UsageRecorded { .. } => 3,
            Self::UserInput { .. }
            | Self::Thinking { .. }
            | Self::AssistantMessage { .. }
            | Self::ErrorObserved { .. } => 4,
            Self::ToolStarted { .. } | Self::ToolCompleted { .. } => 5,
        }
    }
}

/// One fused, deduplicated event in a terminal's chronological activity report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusedAgentEvent {
    /// Deterministic deduplication identity key.
    pub canonical_key: String,
    /// UTC event timestamp in milliseconds when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
    /// Derived provenance fidelity ([`EventFidelity::SessionEnriched`] when native data is present).
    pub fidelity: EventFidelity,
    /// Set of sources (`HerdrPty`, `NativeSession`, `SpawnControl`) that observed this event.
    pub origins: BTreeSet<EventOrigin>,
    /// Strongly typed normalized event payload.
    pub kind: FusedEventKind,
    /// Native session ID associated with the event when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Target Herdr terminal address (`t1` or `machine/t1`) when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_address: Option<String>,
}

/// Silence threshold (`5 minutes`) past which a busy agent with no active child command
/// and no recent events is classified as [`AgentHealth::Stalled`].
pub const STALL_THRESHOLD_MS: i64 = 300_000;

/// Closed enumeration of telemetry sources contributing to agent observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservabilitySource {
    /// Live PTY/VT screen buffer witness.
    HerdrPty,
    /// Native agent JSONL session/transcript log.
    NativeSession,
    /// Spawn control-plane or queue command ledger.
    SpawnControl,
    /// Live OS `/proc` process tree & RSS telemetry.
    ProcTree,
    /// Terminal OSC 7501 structured status escapes.
    Osc7501,
}

/// Explicit diagnostic status explaining native session discovery outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionDiscoveryStatus {
    /// Matched and verified a native session file via content sniffing.
    Attached {
        /// Discovery tier that located the file.
        method: SessionDiscoveryMethod,
        /// Verification confidence level.
        confidence: DiscoveryConfidence,
    },
    /// Terminal has no live leader PID (e.g. exited or detached).
    NoLivePid,
    /// Remote federated terminal was not scanned via local `/proc`.
    RemoteTerminalUnscanned,
    /// Scanned `scanned_roots` candidate paths without finding a matching session file.
    NoMatchingSessionFile {
        /// Count of unique roots scanned across open FDs, env roots, and hints.
        scanned_roots: usize,
    },
    /// Discovered a matching session file, but reading/parsing its tail failed.
    ReadFailed {
        /// Diagnostic failure description (redacted/bounded).
        reason: String,
    },
}

impl SessionDiscoveryStatus {
    /// Concise human-readable status summary string.
    #[must_use]
    pub fn short_reason(&self) -> String {
        match self {
            Self::Attached { method, confidence } => format!("attached({method:?}/{confidence:?})"),
            Self::NoLivePid => "no_live_pid".to_string(),
            Self::RemoteTerminalUnscanned => "remote_unscanned".to_string(),
            Self::NoMatchingSessionFile { scanned_roots } => {
                format!("no_session_file(roots={scanned_roots})")
            }
            Self::ReadFailed { reason } => format!("read_failed({reason})"),
        }
    }
}

/// Snapshot of one live OS process inside a terminal's `/proc` subtree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessNode {
    /// Process ID.
    pub pid: u32,
    /// Parent process ID.
    pub ppid: u32,
    /// Kernel `comm` executable short name.
    pub comm: String,
    /// Kernel single-letter state (`S`, `R`, `D`, ...).
    pub state: String,
    /// Resident set size (`VmRSS`) in KiB.
    pub rss_kib: u64,
    /// Redacted, bounded command line summary.
    pub command: String,
}

/// Aggregated `/proc` process-tree telemetry for a live terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTreeTelemetry {
    /// PTY foreground/leader process ID.
    pub leader_pid: u32,
    /// Total live non-zombie processes rooted at `leader_pid`.
    pub process_count: usize,
    /// Number of descendant child processes (`process_count.saturating_sub(1)`).
    pub child_count: usize,
    /// Sum of resident memory (`VmRSS`) across `processes` in KiB.
    pub total_rss_kib: u64,
    /// Deepest non-wrapper child command currently running, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_leaf_command: Option<String>,
    /// Sampled live processes in BFS tree order.
    #[serde(default)]
    pub processes: Vec<ProcessNode>,
}

/// Compact per-terminal telemetry attached to [`crate::tui::DiscoveredTerminal`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTelemetrySummary {
    /// Live non-zombie process count in the terminal subtree.
    pub process_count: usize,
    /// Descendant child process count (`process_count.saturating_sub(1)`).
    pub child_count: usize,
    /// Combined resident memory in KiB across sampled processes.
    pub total_rss_kib: u64,
    /// Active leaf child command currently executing, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_command: Option<String>,
    /// Explicit diagnostic status of native session file discovery.
    pub discovery_status: SessionDiscoveryStatus,
    /// Size in bytes of the discovered session file when stat'd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_bytes: Option<u64>,
    /// Last modification UTC timestamp in milliseconds of the discovered session file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_mtime_ms: Option<i64>,
}

/// Tool invocation observed as started without a subsequent completion record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlightTool {
    /// Native tool call ID if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool name (`Bash`, `Read`, ...).
    pub tool_name: String,
    /// Bounded, redacted summary of tool input arguments.
    pub summary: String,
    /// UTC start timestamp in milliseconds when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
}

/// Cumulative token accounting rolled up across observed usage records.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Most recently observed model identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    /// Cumulative input/prompt tokens.
    pub input_tokens: u64,
    /// Cumulative output/completion tokens.
    pub output_tokens: u64,
    /// Cumulative prompt cache read tokens.
    pub cache_read_tokens: u64,
    /// Cumulative prompt cache creation/write tokens.
    pub cache_write_tokens: u64,
}

/// Closed health classification derived from live PTY state, `/proc` activity,
/// session discovery outcome, and fused event stream signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "health", rename_all = "snake_case")]
pub enum AgentHealth {
    /// Live and idle with no active blocker or unresolved error tail.
    Healthy,
    /// Actively executing a turn or running a child tool.
    Busy,
    /// Waiting on a live interactive prompt/dialog blocker.
    Blocked {
        /// Matching screen/witness blocker rule.
        rule: String,
    },
    /// Working status with no active child leaf command and extended silence (`>= 5m`).
    Stalled {
        /// Elapsed milliseconds since `last_event_at_ms`.
        silent_ms: u64,
    },
    /// Recent unresolved errors observed in session/tool execution.
    Erroring {
        /// Total observed turn/stream errors plus failed tool calls.
        error_count: usize,
    },
    /// Partial observability degradation (e.g. session log read failure).
    Degraded {
        /// Redacted degradation reason.
        reason: String,
    },
    /// Backing terminal process has exited.
    Exited,
}

/// Rolled-up structured observability summary computed across full fused events
/// and host `/proc` telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentObservabilitySummary {
    /// Derived health verdict.
    pub health: AgentHealth,
    /// Native session discovery status.
    pub discovery_status: SessionDiscoveryStatus,
    /// Distinct telemetry sources contributing to this report.
    pub sources_observed: BTreeSet<ObservabilitySource>,
    /// Total completed turns seen in the event window.
    pub turn_count: usize,
    /// Total tool invocations started in the event window.
    pub tool_call_count: usize,
    /// Total tool calls ending with failure (`ok == false`).
    pub tool_error_count: usize,
    /// Total turn/stream/system errors observed.
    pub error_count: usize,
    /// Total interactive blocker signals observed.
    pub blocked_count: usize,
    /// Tools started without a matching completion in the window.
    #[serde(default)]
    pub in_flight_tools: Vec<InFlightTool>,
    /// Cumulative token usage across observed checkpoints.
    pub usage_totals: UsageTotals,
    /// Earliest UTC event timestamp in the full fused window, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event_at_ms: Option<i64>,
    /// Latest UTC event timestamp in the full fused window, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at_ms: Option<i64>,
    /// Most recent tool name invoked or completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    /// Most recent redacted error message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Pure input bundle passed to [`summarize_agent_observability`].
pub struct SummaryInput<'a> {
    /// Full chronological fused event list prior to `after_key`/`limit` slicing.
    pub events: &'a [FusedAgentEvent],
    /// Whether the underlying Herdr terminal process is still live.
    pub is_live: bool,
    /// Current screen/witness coarse status.
    pub agent_status: crate::port::AgentStatus,
    /// Matching screen/witness rule name if any.
    pub active_rule: Option<&'a str>,
    /// Whether live OSC 7501 status metadata is present.
    pub has_osc_signal: bool,
    /// Optional `/proc` tree telemetry snapshot.
    pub process_tree: Option<&'a ProcessTreeTelemetry>,
    /// Outcome of native session discovery & log reading.
    pub discovery_status: SessionDiscoveryStatus,
    /// Explicit caller-supplied UTC millisecond timestamp for stall calculation.
    pub now_ms: Option<i64>,
}

/// Complete fused event report returned by `HerdrGlobalClient::events_scoped`,
/// `HerdrGlobalClient::events_scoped_since`, `spawn agent events`, and `GET /api/agents/<addr>/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusedEventsReport {
    /// Resolved Herdr terminal address.
    pub address: String,
    /// Detected running agent engine, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Auto-discovered native session attachment when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionAttachment>,
    /// Overall report fidelity (`SessionEnriched` if any session-enriched event exists, else `ScreenOnly` / `ControlOnly`).
    pub fidelity: EventFidelity,
    /// Rolled-up structured health, tool, usage, and discovery summary.
    pub summary: AgentObservabilitySummary,
    /// Optional `/proc` subtree telemetry for live local terminals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_tree: Option<ProcessTreeTelemetry>,
    /// Canonical key of the newest event in the full fused stream (cursor for `--after-key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_key: Option<String>,
    /// True when the requested `after_key` cursor was not found in the current window.
    #[serde(default)]
    pub cursor_reset: bool,
    /// Number of returned events in `events`.
    pub count: usize,
    /// Whether older events were omitted by the requested `limit`.
    pub truncated: bool,
    /// Chronologically ordered fused events.
    pub events: Vec<FusedAgentEvent>,
}

/// Metadata extracted during content-based schema sniffing of a candidate session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniffedSessionMeta {
    /// Matched session format.
    pub format: SessionFileFormat,
    /// Native session/conversation identifier if extracted from the head lines.
    pub native_session_id: Option<String>,
    /// Workspace `cwd` recorded in the head lines, if any.
    pub cwd: Option<String>,
    /// First valid UTC timestamp in milliseconds found in the head lines.
    pub timestamp_ms: Option<i64>,
}

/// Collapse whitespace, redact secrets via [`redact`], and bound length to
/// [`MAX_SUMMARY_CHARS`] Unicode scalar values.
#[must_use]
pub fn clean_summary(raw: &str) -> String {
    let redacted = redact(raw);
    let collapsed = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (count, ch) in collapsed.chars().enumerate() {
        if count >= MAX_SUMMARY_CHARS {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn non_empty_str(val: Option<&Value>) -> Option<String> {
    val.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn timestamp_ms_from_value(val: &Value) -> Option<i64> {
    for key in ["timestamp", "created_at", "started_at", "completed_at"] {
        if let Some(raw) = val.get(key) {
            if let Some(ms) = raw.as_i64() {
                if ms > 1_000_000_000_000 {
                    return Some(ms);
                }
                if ms > 1_000_000_000 {
                    return Some(ms.saturating_mul(1000));
                }
            }
            if let Some(text) = raw.as_str()
                && let Some(ms) = utc_ms(text)
            {
                return Some(ms);
            }
        }
    }
    None
}

fn fnv1a_hex(text: &str) -> String {
    let digest = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{digest:016x}")
}

fn classify_json_line(obj: &Value) -> Option<SniffedSessionMeta> {
    let typ = obj.get("type").and_then(Value::as_str);
    if matches!(
        typ,
        Some("session_meta" | "turn_context" | "event_msg" | "response_item" | "world_state")
    ) && obj.get("payload").is_some_and(Value::is_object)
    {
        let payload = obj.get("payload");
        let sid = non_empty_str(payload.and_then(|p| p.get("session_id").or_else(|| p.get("id"))))
            .or_else(|| non_empty_str(obj.get("session_id")));
        let cwd = non_empty_str(payload.and_then(|p| {
            p.get("cwd")
                .or_else(|| p.pointer("/runtime_workspace_roots/0"))
        }));
        return Some(SniffedSessionMeta {
            format: SessionFileFormat::CodexRolloutJsonl,
            native_session_id: sid,
            cwd,
            timestamp_ms: timestamp_ms_from_value(obj),
        });
    }
    if matches!(
        typ,
        Some(
            "custom-title"
                | "atis-latch"
                | "tools-used"
                | "user"
                | "assistant"
                | "cost-state"
                | "queue-operation"
                | "api-request"
                | "api-request-shape"
                | "api-request-blob"
                | "last-prompt"
                | "attachment"
                | "system"
        )
    ) && (obj.get("sessionId").is_some()
        || (typ == Some("system") && obj.get("retryAttempt").is_some()))
    {
        return Some(SniffedSessionMeta {
            format: SessionFileFormat::ClaudeJsonl,
            native_session_id: non_empty_str(obj.get("sessionId")),
            cwd: non_empty_str(obj.get("cwd")),
            timestamp_ms: timestamp_ms_from_value(obj),
        });
    }
    if matches!(
        typ,
        Some("USER_INPUT" | "PLANNER_RESPONSE" | "SYSTEM_MESSAGE" | "GENERIC" | "CHECKPOINT")
    ) && obj.get("step_index").is_some()
        && obj.get("created_at").is_some()
    {
        return Some(SniffedSessionMeta {
            format: SessionFileFormat::AgyFullJsonl,
            native_session_id: non_empty_str(obj.get("session_id")),
            cwd: None,
            timestamp_ms: timestamp_ms_from_value(obj),
        });
    }
    if obj.get("sessionID").is_some()
        || (obj.get("part").is_some_and(Value::is_object) && obj.get("type").is_some())
    {
        return Some(SniffedSessionMeta {
            format: SessionFileFormat::OpenCodeJsonl,
            native_session_id: non_empty_str(
                obj.get("sessionID")
                    .or_else(|| obj.pointer("/part/sessionID")),
            ),
            cwd: non_empty_str(obj.get("cwd").or_else(|| obj.pointer("/part/cwd"))),
            timestamp_ms: timestamp_ms_from_value(obj),
        });
    }
    let has_discriminant = obj
        .get("type")
        .or_else(|| obj.get("event"))
        .or_else(|| obj.get("kind"))
        .is_some_and(Value::is_string);
    let has_agent_field = ["role", "message", "tool", "tool_name", "usage", "turn_id"]
        .iter()
        .any(|k| obj.get(*k).is_some());
    if has_discriminant && has_agent_field {
        return Some(SniffedSessionMeta {
            format: SessionFileFormat::GenericJsonl,
            native_session_id: non_empty_str(
                obj.get("session_id")
                    .or_else(|| obj.get("sessionId"))
                    .or_else(|| obj.get("thread_id")),
            ),
            cwd: non_empty_str(obj.get("cwd")),
            timestamp_ms: timestamp_ms_from_value(obj),
        });
    }
    None
}

/// Inspect head lines from a candidate session file and determine its
/// [`SessionFileFormat`] + extracted session ID / `cwd` / start timestamp.
///
/// Rejects non-session JSON objects (such as `package.json` or settings files).
#[must_use]
pub fn sniff_session_metadata(head_lines: &[&str]) -> Option<SniffedSessionMeta> {
    let mut best: Option<SniffedSessionMeta> = None;
    for line in head_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(meta) = classify_json_line(&obj) else {
            continue;
        };
        match &mut best {
            None => best = Some(meta),
            Some(prev) => {
                if prev.format == SessionFileFormat::GenericJsonl
                    && meta.format != SessionFileFormat::GenericJsonl
                {
                    prev.format = meta.format;
                }
                if prev.native_session_id.is_none() {
                    prev.native_session_id = meta.native_session_id;
                }
                if prev.cwd.is_none() {
                    prev.cwd = meta.cwd;
                }
                if prev.timestamp_ms.is_none() {
                    prev.timestamp_ms = meta.timestamp_ms;
                }
            }
        }
        if let Some(cur) = &best
            && cur.format != SessionFileFormat::GenericJsonl
            && cur.native_session_id.is_some()
            && cur.cwd.is_some()
        {
            break;
        }
    }
    best
}

#[cfg(test)]
#[must_use]
fn sniff_session_format(head_lines: &[&str]) -> Option<SessionFileFormat> {
    sniff_session_metadata(head_lines).map(|m| m.format)
}

fn stable_line_fallback(prefix: &str, ts: Option<i64>, raw_line: &str) -> String {
    let hash = fnv1a_hex(raw_line);
    match ts {
        Some(t) => format!("{prefix}:t{t}:{hash}"),
        None => format!("{prefix}:h:{hash}"),
    }
}

fn make_session_event(
    canonical_key: &CanonicalEventKey,
    timestamp_ms: Option<i64>,
    kind: FusedEventKind,
    session_id: Option<String>,
    terminal_address: Option<&str>,
) -> FusedAgentEvent {
    FusedAgentEvent {
        canonical_key: canonical_key.canonical_string(),
        timestamp_ms,
        fidelity: EventFidelity::SessionEnriched,
        origins: BTreeSet::from([EventOrigin::NativeSession]),
        kind,
        session_id,
        terminal_address: terminal_address.map(str::to_string),
    }
}

fn parse_claude_line(
    obj: &Value,
    raw_line: &str,
    addr: Option<&str>,
    fallback_sid: Option<&str>,
) -> Vec<FusedAgentEvent> {
    let mut out = Vec::new();
    let ts = timestamp_ms_from_value(obj);
    let sid = non_empty_str(obj.get("sessionId")).or_else(|| fallback_sid.map(str::to_string));
    let base_key = non_empty_str(obj.get("uuid"))
        .unwrap_or_else(|| stable_line_fallback("claude", ts, raw_line));

    match obj.get("type").and_then(Value::as_str) {
        Some("user") => parse_claude_user(obj, ts, &base_key, sid, addr, &mut out),
        Some("assistant") => parse_claude_assistant(obj, ts, &base_key, sid, addr, &mut out),
        Some("system") => parse_claude_system(obj, ts, &base_key, sid, addr, &mut out),
        Some("cost-state") => {
            if let Some(usage) = parse_claude_cost_usage(obj) {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: "claude:cost-state".to_string(),
                    },
                    ts,
                    usage,
                    sid,
                    addr,
                ));
            }
        }
        _ => {}
    }
    out
}

fn parse_claude_system(
    obj: &Value,
    ts: Option<i64>,
    base_key: &str,
    sid: Option<String>,
    addr: Option<&str>,
    out: &mut Vec<FusedAgentEvent>,
) {
    let Some(err_val) = obj.get("error").filter(|v| !v.is_null()) else {
        return;
    };
    let raw_msg = non_empty_str(
        err_val
            .get("formatted")
            .or_else(|| err_val.get("message"))
            .or(Some(err_val)),
    )
    .unwrap_or_else(|| clean_summary(&err_val.to_string()));
    if raw_msg.is_empty() {
        return;
    }
    let status_part = err_val
        .get("status")
        .and_then(Value::as_i64)
        .map(|s| format!("status={s} "))
        .unwrap_or_default();
    let retry_part = match (
        obj.get("retryAttempt").and_then(Value::as_u64),
        obj.get("maxRetries").and_then(Value::as_u64),
    ) {
        (Some(a), Some(m)) => format!("(retry {a}/{m}) "),
        _ => String::new(),
    };
    let message = clean_summary(&format!("{status_part}{retry_part}{raw_msg}"));
    if !message.is_empty() {
        out.push(make_session_event(
            &CanonicalEventKey::NativeRecord {
                source_id: format!("{base_key}:sys_err"),
            },
            ts,
            FusedEventKind::ErrorObserved { message },
            sid,
            addr,
        ));
    }
}

fn extract_text_content(val: Option<&Value>) -> String {
    match val {
        Some(Value::String(s)) => clean_summary(s),
        Some(Value::Array(items)) => {
            let joined: Vec<&str> = items
                .iter()
                .filter_map(|item| item.get("text").or_else(|| item.get("input_text")))
                .filter_map(Value::as_str)
                .collect();
            clean_summary(&joined.join(" "))
        }
        _ => String::new(),
    }
}

fn parse_claude_user(
    obj: &Value,
    ts: Option<i64>,
    base_key: &str,
    sid: Option<String>,
    addr: Option<&str>,
    out: &mut Vec<FusedAgentEvent>,
) {
    let Some(content) = obj.pointer("/message/content") else {
        return;
    };
    if let Value::String(text) = content {
        let summary = clean_summary(text);
        if !summary.is_empty() {
            out.push(make_session_event(
                &CanonicalEventKey::NativeRecord {
                    source_id: base_key.to_string(),
                },
                ts,
                FusedEventKind::UserInput { summary },
                sid,
                addr,
            ));
        }
        return;
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    let mut text_parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                if let Some(id) = non_empty_str(block.get("tool_use_id")) {
                    let is_err = block.get("is_error").and_then(Value::as_bool) == Some(true);
                    let summary = extract_text_content(block.get("content"));
                    out.push(make_session_event(
                        &CanonicalEventKey::ToolCall {
                            id: id.clone(),
                            phase: ToolPhase::Completed,
                        },
                        ts,
                        FusedEventKind::ToolCompleted {
                            tool_call_id: Some(id),
                            tool_name: String::new(),
                            ok: !is_err,
                            duration_ms: None,
                            summary,
                        },
                        sid.clone(),
                        addr,
                    ));
                }
            }
            Some("text") | None => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(text);
                }
            }
            _ => {}
        }
    }
    let combined = clean_summary(&text_parts.join(" "));
    if !combined.is_empty() {
        out.push(make_session_event(
            &CanonicalEventKey::NativeRecord {
                source_id: format!("{base_key}:user"),
            },
            ts,
            FusedEventKind::UserInput { summary: combined },
            sid,
            addr,
        ));
    }
}

struct LineEventCtx<'a> {
    ts: Option<i64>,
    line_key: &'a str,
    sid: Option<String>,
    addr: Option<&'a str>,
}

fn parse_claude_assistant(
    obj: &Value,
    ts: Option<i64>,
    base_key: &str,
    sid: Option<String>,
    addr: Option<&str>,
    out: &mut Vec<FusedAgentEvent>,
) {
    let model = non_empty_str(
        obj.pointer("/message/model")
            .or_else(|| obj.get("requestedModel")),
    );
    let message_id = non_empty_str(obj.pointer("/message/id"));
    let thinking_ms = obj.get("thinkingDurationMs").and_then(Value::as_u64);
    let ctx = LineEventCtx {
        ts,
        line_key: base_key,
        sid,
        addr,
    };
    if let Some(blocks) = obj.pointer("/message/content").and_then(Value::as_array) {
        for (idx, block) in blocks.iter().enumerate() {
            parse_claude_assistant_block(block, idx, thinking_ms, &ctx, out);
        }
    }
    if let Some(usage) = obj.pointer("/message/usage") {
        parse_claude_assistant_usage(usage, message_id.as_deref(), model, &ctx, out);
    }
    if let Some(reason @ ("end_turn" | "max_tokens")) =
        obj.pointer("/message/stop_reason").and_then(Value::as_str)
    {
        let stop_source = message_id.as_deref().map_or_else(
            || format!("{base_key}:stop"),
            |mid| format!("claude:msg:{mid}:stop"),
        );
        out.push(make_session_event(
            &CanonicalEventKey::NativeRecord {
                source_id: stop_source,
            },
            ts,
            FusedEventKind::TurnCompleted {
                turn_id: message_id,
                duration_ms: thinking_ms,
                error: (reason == "max_tokens").then(|| "max_tokens".to_string()),
            },
            ctx.sid,
            addr,
        ));
    }
}

fn parse_claude_assistant_block(
    block: &Value,
    idx: usize,
    thinking_ms: Option<u64>,
    ctx: &LineEventCtx<'_>,
    out: &mut Vec<FusedAgentEvent>,
) {
    match block.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            let summary = clean_summary(
                block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            if !summary.is_empty() {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: format!("{}:think:{idx}", ctx.line_key),
                    },
                    ctx.ts,
                    FusedEventKind::Thinking {
                        summary,
                        duration_ms: thinking_ms,
                    },
                    ctx.sid.clone(),
                    ctx.addr,
                ));
            }
        }
        Some("text") => {
            let summary = clean_summary(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            if !summary.is_empty() {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: format!("{}:text:{idx}", ctx.line_key),
                    },
                    ctx.ts,
                    FusedEventKind::AssistantMessage { summary },
                    ctx.sid.clone(),
                    ctx.addr,
                ));
            }
        }
        Some("tool_use") => {
            let tool_name = non_empty_str(block.get("name")).unwrap_or_else(|| "tool".into());
            let tool_id = non_empty_str(block.get("id"));
            let input_preview = block
                .get("input")
                .map(|v| clean_summary(&v.to_string()))
                .unwrap_or_default();
            let key = tool_id.as_ref().map_or_else(
                || CanonicalEventKey::NativeRecord {
                    source_id: format!("{}:tool:{idx}", ctx.line_key),
                },
                |id| CanonicalEventKey::ToolCall {
                    id: id.clone(),
                    phase: ToolPhase::Started,
                },
            );
            out.push(make_session_event(
                &key,
                ctx.ts,
                FusedEventKind::ToolStarted {
                    tool_call_id: tool_id,
                    tool_name,
                    summary: input_preview,
                },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
        _ => {}
    }
}

fn parse_claude_assistant_usage(
    usage: &Value,
    message_id: Option<&str>,
    model: Option<String>,
    ctx: &LineEventCtx<'_>,
    out: &mut Vec<FusedAgentEvent>,
) {
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if (input, output, cache_read, cache_write) != (0, 0, 0, 0) {
        let source_id = message_id.map_or_else(
            || format!("{}:usage", ctx.line_key),
            |mid| format!("claude:msg:{mid}:usage"),
        );
        out.push(make_session_event(
            &CanonicalEventKey::NativeRecord { source_id },
            ctx.ts,
            FusedEventKind::UsageRecorded {
                model,
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                cumulative: false,
            },
            ctx.sid.clone(),
            ctx.addr,
        ));
    }
}

fn parse_claude_cost_usage(obj: &Value) -> Option<FusedEventKind> {
    let models = obj.get("modelUsage")?.as_object()?;
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut cache_read = 0_u64;
    let mut cache_write = 0_u64;
    let mut first_model = None;
    for (name, stats) in models {
        if first_model.is_none() {
            first_model = Some(name.clone());
        }
        input = input.saturating_add(
            stats
                .get("inputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        output = output.saturating_add(
            stats
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        cache_read = cache_read.saturating_add(
            stats
                .get("cacheReadInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        cache_write = cache_write.saturating_add(
            stats
                .get("cacheCreationInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    }
    if (input, output, cache_read, cache_write) == (0, 0, 0, 0) {
        return None;
    }
    Some(FusedEventKind::UsageRecorded {
        model: first_model,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cumulative: true,
    })
}

fn parse_codex_rollout_line(
    obj: &Value,
    raw_line: &str,
    addr: Option<&str>,
    fallback_sid: Option<&str>,
) -> Vec<FusedAgentEvent> {
    let mut out = Vec::new();
    let ts = timestamp_ms_from_value(obj);
    let payload = obj.get("payload");
    let sid = non_empty_str(payload.and_then(|p| p.get("session_id").or_else(|| p.get("id"))))
        .or_else(|| fallback_sid.map(str::to_string));
    let line_id = obj.get("ordinal").and_then(Value::as_u64).map_or_else(
        || stable_line_fallback("codex", ts, raw_line),
        |n| format!("codex:o{n}:{}", fnv1a_hex(raw_line)),
    );
    let ctx = LineEventCtx {
        ts,
        line_key: &line_id,
        sid: sid.clone(),
        addr,
    };

    match obj.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            if let Some(p) = payload {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: format!("{line_id}:meta"),
                    },
                    ts,
                    FusedEventKind::SessionMeta {
                        engine: Some("codex".to_string()),
                        cli_version: non_empty_str(p.get("cli_version")),
                        cwd: non_empty_str(
                            p.get("cwd")
                                .or_else(|| p.pointer("/runtime_workspace_roots/0")),
                        ),
                        model: non_empty_str(p.get("model_provider")),
                    },
                    sid,
                    addr,
                ));
            }
        }
        Some("turn_context") => {
            if let Some(p) = payload {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: format!("{line_id}:ctx"),
                    },
                    ts,
                    FusedEventKind::SessionMeta {
                        engine: Some("codex".to_string()),
                        cli_version: None,
                        cwd: non_empty_str(
                            p.get("cwd")
                                .or_else(|| p.pointer("/runtime_workspace_roots/0")),
                        ),
                        model: non_empty_str(p.get("model")),
                    },
                    sid,
                    addr,
                ));
            }
        }
        Some("event_msg") => {
            if let Some(p) = payload {
                parse_codex_event_msg(p, &ctx, &mut out);
            }
        }
        Some("response_item") => {
            if let Some(p) = payload {
                parse_codex_response_item(p, &ctx, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn extract_codex_error_text(val: Option<&Value>) -> Option<String> {
    let err = val.filter(|v| !v.is_null())?;
    let raw = non_empty_str(
        err.get("message")
            .or_else(|| err.get("codex_error_info"))
            .or(Some(err)),
    )
    .or_else(|| Some(err.to_string()))?;
    let cleaned = clean_summary(&raw);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn parse_codex_event_msg(p: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    let sub = p.get("type").and_then(Value::as_str);
    let turn_id = non_empty_str(p.get("turn_id").or_else(|| p.get("root_turn_id")));
    match sub {
        Some("task_started") => out.push(make_session_event(
            &CanonicalEventKey::NativeRecord {
                source_id: format!(
                    "codex:turn_start:{}",
                    turn_id.as_deref().unwrap_or(ctx.line_key)
                ),
            },
            ctx.ts.or_else(|| timestamp_ms_from_value(p)),
            FusedEventKind::TurnStarted { turn_id },
            ctx.sid.clone(),
            ctx.addr,
        )),
        Some("task_complete" | "turn_complete") => {
            let err = extract_codex_error_text(p.get("error"));
            out.push(make_session_event(
                &CanonicalEventKey::NativeRecord {
                    source_id: format!(
                        "codex:turn_end:{}",
                        turn_id.as_deref().unwrap_or(ctx.line_key)
                    ),
                },
                ctx.ts.or_else(|| timestamp_ms_from_value(p)),
                FusedEventKind::TurnCompleted {
                    turn_id,
                    duration_ms: p.get("duration_ms").and_then(Value::as_u64),
                    error: err,
                },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
        Some("user_message") => {
            let summary = clean_summary(
                p.get("message")
                    .or_else(|| p.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            if !summary.is_empty() {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: format!("{}:user", ctx.line_key),
                    },
                    ctx.ts,
                    FusedEventKind::UserInput { summary },
                    ctx.sid.clone(),
                    ctx.addr,
                ));
            }
        }
        Some("token_count") => parse_codex_token_count(p, ctx, out),
        Some("error" | "stream_error") => {
            let msg = extract_codex_error_text(p.get("message").or_else(|| p.get("error")))
                .unwrap_or_else(|| "codex error".to_string());
            out.push(make_session_event(
                &CanonicalEventKey::NativeRecord {
                    source_id: format!("{}:err", ctx.line_key),
                },
                ctx.ts,
                FusedEventKind::ErrorObserved { message: msg },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
        Some("item_completed") => parse_codex_item_completed(p, ctx, out),
        _ => {}
    }
}

fn parse_codex_token_count(p: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    let Some(u) = p
        .pointer("/info/last_token_usage")
        .or_else(|| p.get("last_token_usage"))
    else {
        return;
    };
    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if (input, output, cached) != (0, 0, 0) {
        out.push(make_session_event(
            &CanonicalEventKey::NativeRecord {
                source_id: format!("{}:tokens", ctx.line_key),
            },
            ctx.ts,
            FusedEventKind::UsageRecorded {
                model: non_empty_str(p.get("model")),
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                cumulative: false,
            },
            ctx.sid.clone(),
            ctx.addr,
        ));
    }
}

fn parse_codex_item_completed(p: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    let Some(item) = p.get("item") else {
        return;
    };
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return;
    };
    let item_ts = p.get("completed_at_ms").and_then(Value::as_i64).or(ctx.ts);
    let started_ms = p.get("started_at_ms").and_then(Value::as_i64);
    let duration_ms =
        item.get("duration_ms")
            .and_then(Value::as_u64)
            .or_else(|| match (started_ms, item_ts) {
                (Some(s), Some(e)) => u64::try_from(e.saturating_sub(s)).ok(),
                _ => None,
            });
    let item_id = non_empty_str(item.get("id"));
    match kind {
        "agent_message" | "assistant_message" => {
            let from_content = extract_text_content(item.get("content"));
            let summary = if from_content.is_empty() {
                clean_summary(
                    item.get("text")
                        .or_else(|| item.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            } else {
                from_content
            };
            if !summary.is_empty() {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: item_id.unwrap_or_else(|| format!("{}:item", ctx.line_key)),
                    },
                    item_ts,
                    FusedEventKind::AssistantMessage { summary },
                    ctx.sid.clone(),
                    ctx.addr,
                ));
            }
        }
        "error" => {
            let message =
                extract_codex_error_text(item.get("message").or_else(|| item.get("text")))
                    .unwrap_or_else(|| "codex item error".to_string());
            out.push(make_session_event(
                &CanonicalEventKey::NativeRecord {
                    source_id: item_id.unwrap_or_else(|| format!("{}:item_err", ctx.line_key)),
                },
                item_ts,
                FusedEventKind::ErrorObserved { message },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
        "command_execution" | "file_change" | "apply_patch" | "mcp_tool_call" | "web_search" => {
            let tool_name = match kind {
                "command_execution" => "Bash".to_string(),
                "file_change" | "apply_patch" => "Edit".to_string(),
                "web_search" => "WebSearch".to_string(),
                _ => non_empty_str(item.get("tool").or_else(|| item.get("name")))
                    .unwrap_or_else(|| "mcp".to_string()),
            };
            let status = item.get("status").and_then(Value::as_str);
            let exit_code = item.get("exit_code").and_then(Value::as_i64);
            let err = item.get("error").filter(|v| !v.is_null());
            let ok = status != Some("failed") && exit_code.is_none_or(|c| c == 0) && err.is_none();
            let raw_summary = non_empty_str(
                err.and_then(|e| e.get("message").or(Some(e)))
                    .or_else(|| item.get("aggregated_output"))
                    .or_else(|| item.get("command"))
                    .or_else(|| item.get("path"))
                    .or_else(|| item.get("query")),
            )
            .unwrap_or_else(|| kind.to_string());
            let key = item_id.as_ref().map_or_else(
                || CanonicalEventKey::NativeRecord {
                    source_id: format!("{}:item_tool", ctx.line_key),
                },
                |id| CanonicalEventKey::ToolCall {
                    id: id.clone(),
                    phase: ToolPhase::Completed,
                },
            );
            out.push(make_session_event(
                &key,
                item_ts,
                FusedEventKind::ToolCompleted {
                    tool_call_id: item_id,
                    tool_name,
                    ok,
                    duration_ms,
                    summary: clean_summary(&raw_summary),
                },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
        _ => {}
    }
}

fn parse_codex_response_item(p: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    match p.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = p.get("role").and_then(Value::as_str).unwrap_or_default();
            let summary = extract_text_content(p.get("content"));
            if summary.is_empty() {
                return;
            }
            let key = non_empty_str(p.get("id"))
                .unwrap_or_else(|| format!("{}:msg:{role}", ctx.line_key));
            let kind = match role {
                "user" => FusedEventKind::UserInput { summary },
                "assistant" => FusedEventKind::AssistantMessage { summary },
                _ => return,
            };
            out.push(make_session_event(
                &CanonicalEventKey::NativeRecord { source_id: key },
                ctx.ts,
                kind,
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
        Some("reasoning") => {
            let summary = clean_summary(
                p.get("summary")
                    .or_else(|| p.get("text"))
                    .map(ToString::to_string)
                    .unwrap_or_default()
                    .as_str(),
            );
            if !summary.is_empty() {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: non_empty_str(p.get("id"))
                            .unwrap_or_else(|| format!("{}:reason", ctx.line_key)),
                    },
                    ctx.ts,
                    FusedEventKind::Thinking {
                        summary,
                        duration_ms: None,
                    },
                    ctx.sid.clone(),
                    ctx.addr,
                ));
            }
        }
        Some(tag @ ("function_call" | "custom_tool_call" | "local_shell_call")) => {
            parse_codex_tool_call(p, tag, ctx, out);
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            parse_codex_tool_output(p, ctx, out);
        }
        _ => {}
    }
}

fn parse_codex_tool_call(
    p: &Value,
    tag: &str,
    ctx: &LineEventCtx<'_>,
    out: &mut Vec<FusedAgentEvent>,
) {
    let call_id = non_empty_str(p.get("call_id").or_else(|| p.get("id")));
    let name = if tag == "local_shell_call" {
        "Bash".to_string()
    } else {
        non_empty_str(p.get("name")).unwrap_or_else(|| "tool".to_string())
    };
    let args = p
        .get("arguments")
        .or_else(|| p.get("input"))
        .or_else(|| p.get("action"))
        .map(|v| clean_summary(&v.to_string()))
        .unwrap_or_default();
    let key = call_id.as_ref().map_or_else(
        || CanonicalEventKey::NativeRecord {
            source_id: format!("{}:call", ctx.line_key),
        },
        |id| CanonicalEventKey::ToolCall {
            id: id.clone(),
            phase: ToolPhase::Started,
        },
    );
    out.push(make_session_event(
        &key,
        ctx.ts,
        FusedEventKind::ToolStarted {
            tool_call_id: call_id,
            tool_name: name,
            summary: args,
        },
        ctx.sid.clone(),
        ctx.addr,
    ));
}

fn parse_codex_tool_output(p: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    let call_id = non_empty_str(p.get("call_id").or_else(|| p.get("id")));
    let summary = p
        .get("output")
        .map(|v| clean_summary(&v.to_string()))
        .unwrap_or_default();
    let ok = p.get("is_error").and_then(Value::as_bool) != Some(true);
    let key = call_id.as_ref().map_or_else(
        || CanonicalEventKey::NativeRecord {
            source_id: format!("{}:out", ctx.line_key),
        },
        |id| CanonicalEventKey::ToolCall {
            id: id.clone(),
            phase: ToolPhase::Completed,
        },
    );
    out.push(make_session_event(
        &key,
        ctx.ts,
        FusedEventKind::ToolCompleted {
            tool_call_id: call_id,
            tool_name: String::new(),
            ok,
            duration_ms: p.get("duration_ms").and_then(Value::as_u64),
            summary,
        },
        ctx.sid.clone(),
        ctx.addr,
    ));
}

fn parse_agy_line(
    obj: &Value,
    raw_line: &str,
    addr: Option<&str>,
    fallback_sid: Option<&str>,
) -> Vec<FusedAgentEvent> {
    let mut out = Vec::new();
    let ts = timestamp_ms_from_value(obj);
    let sid = non_empty_str(obj.get("session_id")).or_else(|| fallback_sid.map(str::to_string));
    let base_key = obj.get("step_index").and_then(Value::as_u64).map_or_else(
        || stable_line_fallback("agy", ts, raw_line),
        |s| format!("agy:{s}"),
    );
    let ctx = LineEventCtx {
        ts,
        line_key: &base_key,
        sid,
        addr,
    };

    match obj.get("type").and_then(Value::as_str) {
        Some("USER_INPUT") => {
            let summary = clean_summary(
                obj.get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            if !summary.is_empty() {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: base_key.clone(),
                    },
                    ctx.ts,
                    FusedEventKind::UserInput { summary },
                    ctx.sid,
                    ctx.addr,
                ));
            }
        }
        Some("PLANNER_RESPONSE") => parse_agy_planner_response(obj, &ctx, &mut out),
        Some("GENERIC") => parse_agy_generic(obj, &ctx, &mut out),
        Some("CHECKPOINT") => {
            let input = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let output = obj
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = obj
                .get("cache_read_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if (input, output, cached) != (0, 0, 0) {
                out.push(make_session_event(
                    &CanonicalEventKey::NativeRecord {
                        source_id: format!("{base_key}:checkpoint"),
                    },
                    ctx.ts,
                    FusedEventKind::UsageRecorded {
                        model: non_empty_str(obj.get("model")),
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cached,
                        cache_write_tokens: 0,
                        cumulative: false,
                    },
                    ctx.sid,
                    ctx.addr,
                ));
            }
        }
        Some("SYSTEM_MESSAGE") => {
            let status = obj
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase();
            if status.contains("ERR") || status.contains("FAIL") || status.contains("WARN") {
                let message = clean_summary(
                    obj.get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                if !message.is_empty() {
                    out.push(make_session_event(
                        &CanonicalEventKey::NativeRecord {
                            source_id: format!("{base_key}:sys"),
                        },
                        ctx.ts,
                        FusedEventKind::ErrorObserved { message },
                        ctx.sid,
                        ctx.addr,
                    ));
                }
            }
        }
        _ => {}
    }
    out
}

fn parse_agy_generic(obj: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    let summary = clean_summary(
        obj.get("content")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if summary.is_empty() {
        return;
    }
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let source = non_empty_str(obj.get("source")).unwrap_or_else(|| "step".to_string());
    let kind = if status.contains("ERR") || status.contains("FAIL") {
        FusedEventKind::ErrorObserved { message: summary }
    } else if source == "MODEL" {
        FusedEventKind::AssistantMessage { summary }
    } else {
        FusedEventKind::ToolCompleted {
            tool_call_id: None,
            tool_name: source,
            ok: true,
            duration_ms: None,
            summary,
        }
    };
    out.push(make_session_event(
        &CanonicalEventKey::NativeRecord {
            source_id: format!("{}:generic", ctx.line_key),
        },
        ctx.ts,
        kind,
        ctx.sid.clone(),
        ctx.addr,
    ));
}

fn parse_agy_planner_response(obj: &Value, ctx: &LineEventCtx<'_>, out: &mut Vec<FusedAgentEvent>) {
    let model = non_empty_str(obj.get("model"));
    if let Some(thinking) = obj.get("thinking").and_then(Value::as_str) {
        let summary = clean_summary(thinking);
        if !summary.is_empty() {
            out.push(make_session_event(
                &CanonicalEventKey::NativeRecord {
                    source_id: format!("{}:think", ctx.line_key),
                },
                ctx.ts,
                FusedEventKind::Thinking {
                    summary,
                    duration_ms: None,
                },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
    }
    if let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) {
        for (idx, call) in calls.iter().enumerate() {
            let tool_name = non_empty_str(call.get("name")).unwrap_or_else(|| "tool".to_string());
            let tool_id = non_empty_str(call.get("id"));
            let summary = call
                .get("args")
                .map(|v| clean_summary(&v.to_string()))
                .unwrap_or_default();
            let key = tool_id.as_ref().map_or_else(
                || CanonicalEventKey::NativeRecord {
                    source_id: format!("{}:tool:{idx}", ctx.line_key),
                },
                |id| CanonicalEventKey::ToolCall {
                    id: id.clone(),
                    phase: ToolPhase::Started,
                },
            );
            out.push(make_session_event(
                &key,
                ctx.ts,
                FusedEventKind::ToolStarted {
                    tool_call_id: tool_id,
                    tool_name,
                    summary,
                },
                ctx.sid.clone(),
                ctx.addr,
            ));
        }
    }
    let input = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = obj
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = obj
        .get("cache_read_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if (input, output, cached) != (0, 0, 0) {
        out.push(make_session_event(
            &CanonicalEventKey::NativeRecord {
                source_id: format!("{}:usage", ctx.line_key),
            },
            ctx.ts,
            FusedEventKind::UsageRecorded {
                model,
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                cumulative: false,
            },
            ctx.sid.clone(),
            ctx.addr,
        ));
    }
}

fn parse_generic_line(
    obj: &Value,
    raw_line: &str,
    addr: Option<&str>,
    fallback_sid: Option<&str>,
) -> Vec<FusedAgentEvent> {
    let ts = timestamp_ms_from_value(obj);
    let sid = non_empty_str(
        obj.get("session_id")
            .or_else(|| obj.get("sessionId"))
            .or_else(|| obj.get("sessionID"))
            .or_else(|| obj.pointer("/part/sessionID")),
    )
    .or_else(|| fallback_sid.map(str::to_string));
    let key = non_empty_str(obj.get("id").or_else(|| obj.get("uuid")))
        .unwrap_or_else(|| stable_line_fallback("gen", ts, raw_line));

    if let Some(tool) = non_empty_str(
        obj.get("tool")
            .or_else(|| obj.get("tool_name"))
            .or_else(|| obj.pointer("/part/tool")),
    ) {
        let summary = clean_summary(
            &obj.get("input")
                .or_else(|| obj.get("args"))
                .or_else(|| obj.pointer("/part/state/input"))
                .map_or_else(String::new, Value::to_string),
        );
        return vec![make_session_event(
            &CanonicalEventKey::NativeRecord { source_id: key },
            ts,
            FusedEventKind::ToolStarted {
                tool_call_id: non_empty_str(
                    obj.get("call_id").or_else(|| obj.pointer("/part/callID")),
                ),
                tool_name: tool,
                summary,
            },
            sid,
            addr,
        )];
    }

    let role = non_empty_str(obj.get("role")).unwrap_or_default();
    let msg = extract_text_content(
        obj.get("content")
            .or_else(|| obj.get("message"))
            .or_else(|| obj.get("text"))
            .or_else(|| obj.pointer("/part/text")),
    );
    if msg.is_empty() {
        return Vec::new();
    }
    let kind = if role == "user" {
        FusedEventKind::UserInput { summary: msg }
    } else {
        FusedEventKind::AssistantMessage { summary: msg }
    };
    vec![make_session_event(
        &CanonicalEventKey::NativeRecord { source_id: key },
        ts,
        kind,
        sid,
        addr,
    )]
}

/// Parse bounded JSONL lines of a known [`SessionFileFormat`] into normalized,
/// credential-redacted [`FusedAgentEvent`] values.
#[must_use]
pub fn parse_session_lines(
    format: SessionFileFormat,
    lines: &[&str],
    terminal_address: Option<&str>,
    fallback_session_id: Option<&str>,
) -> Vec<FusedAgentEvent> {
    let mut out = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let parsed = match format {
            SessionFileFormat::ClaudeJsonl => {
                parse_claude_line(&obj, trimmed, terminal_address, fallback_session_id)
            }
            SessionFileFormat::CodexRolloutJsonl => {
                parse_codex_rollout_line(&obj, trimmed, terminal_address, fallback_session_id)
            }
            SessionFileFormat::AgyFullJsonl => {
                parse_agy_line(&obj, trimmed, terminal_address, fallback_session_id)
            }
            SessionFileFormat::OpenCodeJsonl | SessionFileFormat::GenericJsonl => {
                parse_generic_line(&obj, trimmed, terminal_address, fallback_session_id)
            }
        };
        out.extend(parsed);
    }
    let mut out = dedup_usage_records(out);
    correlate_tool_names(&mut out);
    out
}

fn merge_usage_kind(existing: &mut FusedEventKind, incoming: FusedEventKind) {
    if let (
        FusedEventKind::UsageRecorded {
            model: ex_model,
            input_tokens: ex_in,
            output_tokens: ex_out,
            cache_read_tokens: ex_cr,
            cache_write_tokens: ex_cw,
            cumulative: ex_cum,
        },
        FusedEventKind::UsageRecorded {
            model: in_model,
            input_tokens: in_in,
            output_tokens: in_out,
            cache_read_tokens: in_cr,
            cache_write_tokens: in_cw,
            cumulative: in_cum,
        },
    ) = (existing, incoming)
    {
        if in_model.is_some() {
            *ex_model = in_model;
        }
        *ex_in = (*ex_in).max(in_in);
        *ex_out = (*ex_out).max(in_out);
        *ex_cr = (*ex_cr).max(in_cr);
        *ex_cw = (*ex_cw).max(in_cw);
        *ex_cum = *ex_cum || in_cum;
    }
}

fn dedup_usage_records(events: Vec<FusedAgentEvent>) -> Vec<FusedAgentEvent> {
    let mut out: Vec<FusedAgentEvent> = Vec::with_capacity(events.len());
    let mut seen_usage: BTreeMap<String, usize> = BTreeMap::new();
    for ev in events {
        if matches!(ev.kind, FusedEventKind::UsageRecorded { .. }) {
            if let Some(&idx) = seen_usage.get(&ev.canonical_key) {
                merge_event(&mut out[idx], ev);
            } else {
                seen_usage.insert(ev.canonical_key.clone(), out.len());
                out.push(ev);
            }
        } else {
            out.push(ev);
        }
    }
    out
}

/// Fill missing `tool_name` on [`FusedEventKind::ToolCompleted`] events from
/// any matching [`FusedEventKind::ToolStarted`] sharing the same `tool_call_id`.
fn correlate_tool_names(events: &mut [FusedAgentEvent]) {
    let mut names = BTreeMap::new();
    for ev in events.iter() {
        if let FusedEventKind::ToolStarted {
            tool_call_id: Some(id),
            tool_name,
            ..
        } = &ev.kind
            && !tool_name.is_empty()
        {
            names.insert(id.clone(), tool_name.clone());
        }
    }
    for ev in events.iter_mut() {
        if let FusedEventKind::ToolCompleted {
            tool_call_id: Some(id),
            tool_name,
            ..
        } = &mut ev.kind
            && tool_name.is_empty()
            && let Some(known) = names.get(id)
        {
            tool_name.clone_from(known);
        }
    }
}

/// Merge a new [`FusedAgentEvent`] into an existing deduplicated entry,
/// forming the union of origins, upgrading fidelity when native evidence meets
/// PTY/control signals, and keeping the richer structured payload.
fn merge_event(existing: &mut FusedAgentEvent, incoming: FusedAgentEvent) {
    existing.origins.extend(incoming.origins);
    if existing.origins.contains(&EventOrigin::NativeSession) {
        existing.fidelity = EventFidelity::SessionEnriched;
    } else if incoming.fidelity > existing.fidelity {
        existing.fidelity = incoming.fidelity;
    }
    existing.timestamp_ms = match (existing.timestamp_ms, incoming.timestamp_ms) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if existing.session_id.is_none() {
        existing.session_id = incoming.session_id;
    }
    if existing.terminal_address.is_none() {
        existing.terminal_address = incoming.terminal_address;
    }
    if matches!(existing.kind, FusedEventKind::UsageRecorded { .. })
        && matches!(incoming.kind, FusedEventKind::UsageRecorded { .. })
    {
        merge_usage_kind(&mut existing.kind, incoming.kind);
    } else if incoming.kind.richness_score() > existing.kind.richness_score() {
        existing.kind = incoming.kind;
    }
}

/// Return `true` when `ev` originates from a timestamped durable session or
/// control-plane record and may anchor incremental `after_key` cursors
/// (never transient PTY screens or timestamp-less snapshots such as Claude `cost-state`).
#[must_use]
pub fn is_cursor_anchor_event(ev: &FusedAgentEvent) -> bool {
    ev.timestamp_ms.is_some()
        && (ev.origins.contains(&EventOrigin::NativeSession)
            || ev.origins.contains(&EventOrigin::SpawnControl))
}

/// Fuse live `HerdrPty`, `NativeSession`, and `SpawnControl` streams via union,
/// deduplication on `canonical_key`, chronological ordering, and tool name correlation,
/// returning the complete fused event timeline prior to cursor/limit slicing.
#[must_use]
pub fn fuse_all_agent_events(
    herdr_events: Vec<FusedAgentEvent>,
    session_events: Vec<FusedAgentEvent>,
    control_events: Vec<FusedAgentEvent>,
) -> Vec<FusedAgentEvent> {
    let mut by_key: BTreeMap<String, (usize, FusedAgentEvent)> = BTreeMap::new();
    let mut seq = 0_usize;

    for ev in session_events
        .into_iter()
        .chain(control_events)
        .chain(herdr_events)
    {
        if let Some((_, existing)) = by_key.get_mut(&ev.canonical_key) {
            merge_event(existing, ev);
        } else {
            let key = ev.canonical_key.clone();
            by_key.insert(key, (seq, ev));
            seq += 1;
        }
    }

    let mut merged: Vec<(usize, FusedAgentEvent)> = by_key.into_values().collect();
    merged.sort_by_key(|(order, ev)| {
        (
            !is_cursor_anchor_event(ev),
            ev.timestamp_ms.unwrap_or(i64::MAX),
            *order,
        )
    });
    let mut events: Vec<FusedAgentEvent> = merged.into_iter().map(|(_, ev)| ev).collect();
    correlate_tool_names(&mut events);
    events
}

/// Slice a chronological fused event list strictly after `after_key` and keep at
/// most `limit` newest events, returning `(events, latest_key, truncated, cursor_reset)`.
///
/// The `latest_key` cursor is anchored strictly to timestamped durable session/control
/// events so neither transient screen updates nor timestamp-less records (such as Claude
/// `cost-state`) hide newly appended records or trigger spurious `cursor_reset` replays.
#[must_use]
pub fn slice_events_after_key(
    events: Vec<FusedAgentEvent>,
    after_key: Option<&str>,
    limit: usize,
) -> (Vec<FusedAgentEvent>, Option<String>, bool, bool) {
    let (mut durable, transient): (Vec<FusedAgentEvent>, Vec<FusedAgentEvent>) =
        events.into_iter().partition(is_cursor_anchor_event);
    let latest_key = durable.last().map(|ev| ev.canonical_key.clone());
    let effective_limit = limit.max(1);
    let mut cursor_reset = false;
    let mut out = if let Some(cursor) = after_key.filter(|k| !k.is_empty()) {
        if let Some(pos) = durable.iter().rposition(|ev| ev.canonical_key == cursor) {
            durable.drain(..=pos);
            durable
        } else {
            cursor_reset = true;
            durable.extend(transient);
            durable
        }
    } else {
        durable.extend(transient);
        durable
    };
    let truncated = out.len() > effective_limit;
    if truncated {
        let drop_count = out.len() - effective_limit;
        out.drain(..drop_count);
    }
    (out, latest_key, truncated, cursor_reset)
}

#[cfg(test)]
#[must_use]
fn fuse_agent_events(
    herdr_events: Vec<FusedAgentEvent>,
    session_events: Vec<FusedAgentEvent>,
    control_events: Vec<FusedAgentEvent>,
    limit: usize,
) -> (Vec<FusedAgentEvent>, bool) {
    let all = fuse_all_agent_events(herdr_events, session_events, control_events);
    let (events, _, truncated, _) = slice_events_after_key(all, None, limit);
    (events, truncated)
}

#[derive(Default)]
struct SummaryAccum {
    sources_observed: BTreeSet<ObservabilitySource>,
    turn_count: usize,
    tool_call_count: usize,
    tool_error_count: usize,
    error_count: usize,
    blocked_count: usize,
    in_flight_tools: Vec<InFlightTool>,
    usage_by_key: BTreeMap<String, FusedEventKind>,
    first_event_at_ms: Option<i64>,
    last_event_at_ms: Option<i64>,
    last_tool_name: Option<String>,
    last_error: Option<String>,
    latest_failed: bool,
}

fn accumulate_event(acc: &mut SummaryAccum, ev: &FusedAgentEvent) {
    accumulate_origins_and_time(acc, ev);
    match &ev.kind {
        FusedEventKind::ProcessState {
            rule: Some(rule), ..
        } if rule.starts_with("osc7501") => {
            acc.sources_observed.insert(ObservabilitySource::Osc7501);
        }
        FusedEventKind::TurnCompleted { error, .. } => {
            acc.turn_count += 1;
            if let Some(err) = error {
                acc.error_count += 1;
                acc.last_error = Some(err.clone());
                acc.latest_failed = true;
            } else {
                acc.latest_failed = false;
            }
        }
        FusedEventKind::ToolStarted { .. } | FusedEventKind::ToolCompleted { .. } => {
            accumulate_tool_event(acc, ev);
        }
        FusedEventKind::UsageRecorded { .. } => {
            accumulate_usage_event(acc, &ev.canonical_key, &ev.kind);
        }
        FusedEventKind::PromptBlocked { .. } => {
            acc.blocked_count += 1;
        }
        FusedEventKind::ErrorObserved { message } => {
            acc.error_count += 1;
            acc.last_error = Some(message.clone());
            acc.latest_failed = true;
        }
        FusedEventKind::AssistantMessage { .. } => {
            acc.latest_failed = false;
        }
        _ => {}
    }
}

fn accumulate_origins_and_time(acc: &mut SummaryAccum, ev: &FusedAgentEvent) {
    for origin in &ev.origins {
        match origin {
            EventOrigin::HerdrPty => {
                acc.sources_observed.insert(ObservabilitySource::HerdrPty);
            }
            EventOrigin::NativeSession => {
                acc.sources_observed
                    .insert(ObservabilitySource::NativeSession);
            }
            EventOrigin::SpawnControl => {
                acc.sources_observed
                    .insert(ObservabilitySource::SpawnControl);
            }
        }
    }
    if let Some(ts) = ev.timestamp_ms {
        acc.first_event_at_ms = Some(acc.first_event_at_ms.map_or(ts, |prev| prev.min(ts)));
        acc.last_event_at_ms = Some(acc.last_event_at_ms.map_or(ts, |prev| prev.max(ts)));
    }
}

fn accumulate_tool_event(acc: &mut SummaryAccum, ev: &FusedAgentEvent) {
    match &ev.kind {
        FusedEventKind::ToolStarted {
            tool_call_id,
            tool_name,
            summary,
        } => {
            acc.tool_call_count += 1;
            acc.last_tool_name = Some(tool_name.clone());
            acc.latest_failed = false;
            let already_tracked = tool_call_id.as_deref().is_some_and(|id| {
                acc.in_flight_tools
                    .iter()
                    .any(|t| t.tool_call_id.as_deref() == Some(id))
            });
            if !already_tracked {
                acc.in_flight_tools.push(InFlightTool {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    summary: summary.clone(),
                    started_at_ms: ev.timestamp_ms,
                });
            }
        }
        FusedEventKind::ToolCompleted {
            tool_call_id,
            tool_name,
            ok,
            summary,
            ..
        } => {
            if !tool_name.is_empty() {
                acc.last_tool_name = Some(tool_name.clone());
            }
            if let Some(id) = tool_call_id {
                acc.in_flight_tools
                    .retain(|t| t.tool_call_id.as_deref() != Some(id.as_str()));
            }
            if *ok {
                acc.latest_failed = false;
            } else {
                acc.tool_error_count += 1;
                if !summary.is_empty() {
                    acc.last_error = Some(summary.clone());
                }
                acc.latest_failed = true;
            }
        }
        _ => {}
    }
}

fn accumulate_usage_event(acc: &mut SummaryAccum, key: &str, kind: &FusedEventKind) {
    if !matches!(kind, FusedEventKind::UsageRecorded { .. }) {
        return;
    }
    if let Some(existing) = acc.usage_by_key.get_mut(key) {
        merge_usage_kind(existing, kind.clone());
    } else {
        acc.usage_by_key.insert(key.to_string(), kind.clone());
    }
}

fn fold_usage_totals(usage_by_key: &BTreeMap<String, FusedEventKind>) -> UsageTotals {
    let mut per_msg = UsageTotals::default();
    let mut max_cum = UsageTotals::default();
    for kind in usage_by_key.values() {
        if let FusedEventKind::UsageRecorded {
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cumulative,
        } = kind
        {
            if *cumulative {
                if let Some(m) = model {
                    max_cum.last_model = Some(m.clone());
                }
                max_cum.input_tokens = max_cum.input_tokens.max(*input_tokens);
                max_cum.output_tokens = max_cum.output_tokens.max(*output_tokens);
                max_cum.cache_read_tokens = max_cum.cache_read_tokens.max(*cache_read_tokens);
                max_cum.cache_write_tokens = max_cum.cache_write_tokens.max(*cache_write_tokens);
            } else {
                if let Some(m) = model {
                    per_msg.last_model = Some(m.clone());
                }
                per_msg.input_tokens = per_msg.input_tokens.saturating_add(*input_tokens);
                per_msg.output_tokens = per_msg.output_tokens.saturating_add(*output_tokens);
                per_msg.cache_read_tokens =
                    per_msg.cache_read_tokens.saturating_add(*cache_read_tokens);
                per_msg.cache_write_tokens = per_msg
                    .cache_write_tokens
                    .saturating_add(*cache_write_tokens);
            }
        }
    }
    UsageTotals {
        last_model: per_msg.last_model.or(max_cum.last_model),
        input_tokens: per_msg.input_tokens.max(max_cum.input_tokens),
        output_tokens: per_msg.output_tokens.max(max_cum.output_tokens),
        cache_read_tokens: per_msg.cache_read_tokens.max(max_cum.cache_read_tokens),
        cache_write_tokens: per_msg.cache_write_tokens.max(max_cum.cache_write_tokens),
    }
}

fn derive_agent_health(input: &SummaryInput<'_>, acc: &SummaryAccum) -> AgentHealth {
    if !input.is_live {
        return AgentHealth::Exited;
    }
    if input.agent_status == crate::port::AgentStatus::Blocked {
        return AgentHealth::Blocked {
            rule: input.active_rule.unwrap_or("blocked").to_string(),
        };
    }
    if let SessionDiscoveryStatus::ReadFailed { reason } = &input.discovery_status {
        return AgentHealth::Degraded {
            reason: reason.clone(),
        };
    }
    let combined_errors = acc.error_count.saturating_add(acc.tool_error_count);
    if acc.latest_failed && combined_errors > 0 {
        return AgentHealth::Erroring {
            error_count: combined_errors,
        };
    }
    if input.agent_status == crate::port::AgentStatus::Working {
        let has_active_child = input
            .process_tree
            .is_some_and(|t| t.child_count > 0 && t.active_leaf_command.is_some());
        if !has_active_child
            && let (Some(now), Some(last)) = (input.now_ms, acc.last_event_at_ms)
            && let Some(diff) = now.checked_sub(last)
            && diff >= STALL_THRESHOLD_MS
        {
            return AgentHealth::Stalled {
                silent_ms: u64::try_from(diff).unwrap_or(u64::MAX),
            };
        }
        return AgentHealth::Busy;
    }
    if combined_errors > 0 && input.agent_status == crate::port::AgentStatus::Unknown {
        return AgentHealth::Erroring {
            error_count: combined_errors,
        };
    }
    AgentHealth::Healthy
}

/// Compute structured [`AgentObservabilitySummary`] across the full fused event
/// stream and live OS `/proc` telemetry without mutating or performing IO.
#[must_use]
pub fn summarize_agent_observability(input: &SummaryInput<'_>) -> AgentObservabilitySummary {
    let mut acc = SummaryAccum::default();
    if input.has_osc_signal {
        acc.sources_observed.insert(ObservabilitySource::Osc7501);
    }
    if input.process_tree.is_some() {
        acc.sources_observed.insert(ObservabilitySource::ProcTree);
    }
    for ev in input.events {
        accumulate_event(&mut acc, ev);
    }
    let health = derive_agent_health(input, &acc);
    AgentObservabilitySummary {
        health,
        discovery_status: input.discovery_status.clone(),
        sources_observed: acc.sources_observed,
        turn_count: acc.turn_count,
        tool_call_count: acc.tool_call_count,
        tool_error_count: acc.tool_error_count,
        error_count: acc.error_count,
        blocked_count: acc.blocked_count,
        in_flight_tools: acc.in_flight_tools,
        usage_totals: fold_usage_totals(&acc.usage_by_key),
        first_event_at_ms: acc.first_event_at_ms,
        last_event_at_ms: acc.last_event_at_ms,
        last_tool_name: acc.last_tool_name,
        last_error: acc.last_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_sniffer_identifies_all_formats_and_rejects_configs() {
        let codex = [
            r#"{"ordinal":0,"timestamp":"2026-11-28T00:10:49.000Z","type":"session_meta","payload":{"id":"01a21f7d-84c7-74c3-bec2-f120a3fe5f0f","cwd":"/data00/repo","cli_version":"0.171.0"}}"#,
        ];
        let cm = sniff_session_metadata(&codex).expect("codex meta");
        assert_eq!(cm.format, SessionFileFormat::CodexRolloutJsonl);
        assert_eq!(
            cm.native_session_id.as_deref(),
            Some("01a21f7d-84c7-74c3-bec2-f120a3fe5f0f")
        );
        assert_eq!(cm.cwd.as_deref(), Some("/data00/repo"));
        assert!(cm.timestamp_ms.is_some());

        let claude = [
            r#"{"type":"user","sessionId":"386979a8-92dc-4a51-81ab-a1ffb57e02d6","cwd":"/tmp/ws","timestamp":"2026-11-28T01:00:00Z","message":{"role":"user","content":"hi"}}"#,
        ];
        assert_eq!(
            sniff_session_format(&claude),
            Some(SessionFileFormat::ClaudeJsonl)
        );

        let agy = [
            r#"{"step_index":1,"type":"USER_INPUT","created_at":"2026-11-28T01:00:00Z","content":"check status"}"#,
        ];
        assert_eq!(
            sniff_session_format(&agy),
            Some(SessionFileFormat::AgyFullJsonl)
        );

        let pkg_json = [r#"{"name":"web","version":"1.0.0","dependencies":{}}"#];
        assert_eq!(sniff_session_metadata(&pkg_json), None);
    }

    #[test]
    fn parsers_and_fusion_union_dedup_correlate_enrich_and_redact() {
        let claude_lines = [
            r#"{"type":"user","uuid":"u1","sessionId":"sid-1","timestamp":"2026-11-28T10:00:00Z","message":{"role":"user","content":"use Bearer secret-tok-123"}}"#,
            r#"{"type":"assistant","uuid":"a1","sessionId":"sid-1","timestamp":"2026-11-28T10:00:01Z","thinkingDurationMs":42,"message":{"model":"claude-sonnet-4","content":[{"type":"thinking","thinking":"inspecting"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            r#"{"type":"user","uuid":"u2","sessionId":"sid-1","timestamp":"2026-11-28T10:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#,
        ];
        let session_evs = parse_session_lines(
            SessionFileFormat::ClaudeJsonl,
            &claude_lines,
            Some("t1"),
            None,
        );

        // Simulated HerdrPty stream includes a duplicate of `toolu_1:started` (screen level)
        // plus a unique `PromptBlocked` visual event that never hits the session transcript.
        let pty_dup = FusedAgentEvent {
            canonical_key: CanonicalEventKey::ToolCall {
                id: "toolu_1".to_string(),
                phase: ToolPhase::Started,
            }
            .canonical_string(),
            timestamp_ms: Some(1_795_860_001_000),
            fidelity: EventFidelity::ScreenOnly,
            origins: BTreeSet::from([EventOrigin::HerdrPty]),
            kind: FusedEventKind::ProcessState {
                status: "working".to_string(),
                rule: Some("bash_spinner".to_string()),
            },
            session_id: None,
            terminal_address: Some("t1".to_string()),
        };
        let pty_blocked = FusedAgentEvent {
            canonical_key: CanonicalEventKey::ScreenSignal {
                digest: 0xdead_beef,
                rule: "permission_prompt".to_string(),
            }
            .canonical_string(),
            timestamp_ms: Some(1_795_860_003_000),
            fidelity: EventFidelity::ScreenOnly,
            origins: BTreeSet::from([EventOrigin::HerdrPty]),
            kind: FusedEventKind::PromptBlocked {
                rule: "permission_prompt".to_string(),
                choices_count: 2,
            },
            session_id: None,
            terminal_address: Some("t1".to_string()),
        };

        let (fused, truncated) =
            fuse_agent_events(vec![pty_dup, pty_blocked], session_evs, Vec::new(), 50);
        assert!(!truncated);

        // UserInput is redacted; no plaintext secret survives.
        let user_ev = fused
            .iter()
            .find(|e| matches!(e.kind, FusedEventKind::UserInput { .. }))
            .expect("user event");
        if let FusedEventKind::UserInput { summary } = &user_ev.kind {
            assert!(!summary.contains("secret-tok-123"), "{summary}");
            assert!(summary.contains("[redacted]"), "{summary}");
        }

        // Duplicate key `tool:toolu_1:started` merged both origins and upgraded to SessionEnriched.
        let tool_start_count = fused
            .iter()
            .filter(|e| e.canonical_key == "tool:toolu_1:started")
            .count();
        assert_eq!(tool_start_count, 1);
        let tool_start = fused
            .iter()
            .find(|e| e.canonical_key == "tool:toolu_1:started")
            .unwrap();
        assert_eq!(tool_start.fidelity, EventFidelity::SessionEnriched);
        assert!(tool_start.origins.contains(&EventOrigin::HerdrPty));
        assert!(tool_start.origins.contains(&EventOrigin::NativeSession));
        assert!(matches!(
            tool_start.kind,
            FusedEventKind::ToolStarted { .. }
        ));

        // ToolCompleted had its empty tool_name correlated back from ToolStarted.
        let tool_done = fused
            .iter()
            .find(|e| e.canonical_key == "tool:toolu_1:completed")
            .expect("tool completed");
        if let FusedEventKind::ToolCompleted { tool_name, ok, .. } = &tool_done.kind {
            assert_eq!(tool_name, "Bash");
            assert!(*ok);
        }

        // Unique HerdrPty visual blocker is retained in the union.
        assert!(fused.iter().any(|e| matches!(
            e.kind,
            FusedEventKind::PromptBlocked {
                choices_count: 2,
                ..
            }
        )));
    }

    #[test]
    fn codex_rollout_and_agy_parsers_extract_events_and_respect_limit() {
        let codex_lines = [
            r#"{"ordinal":0,"timestamp":"2026-11-28T10:00:00Z","type":"session_meta","payload":{"id":"cx-1","cwd":"/ws","cli_version":"0.171.0"}}"#,
            r#"{"ordinal":1,"timestamp":"2026-11-28T10:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-9"}}"#,
            r#"{"ordinal":2,"timestamp":"2026-11-28T10:00:02Z","type":"response_item","payload":{"type":"function_call","call_id":"call_9","name":"shell_command","arguments":"{\"cmd\":\"echo hi\"}"}}"#,
            r#"{"ordinal":3,"timestamp":"2026-11-28T10:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_9","output":"hi"}}"#,
            r#"{"ordinal":4,"timestamp":"2026-11-28T10:00:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-9","duration_ms":3000}}"#,
        ];
        let parsed = parse_session_lines(
            SessionFileFormat::CodexRolloutJsonl,
            &codex_lines,
            Some("t2"),
            None,
        );
        assert_eq!(parsed.len(), 5);
        assert!(matches!(parsed[1].kind, FusedEventKind::TurnStarted { .. }));
        assert!(matches!(
            parsed[4].kind,
            FusedEventKind::TurnCompleted {
                duration_ms: Some(3000),
                ..
            }
        ));

        let (capped, truncated) = fuse_agent_events(Vec::new(), parsed, Vec::new(), 2);
        assert!(truncated);
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn real_n252_claude_codex_and_agy_schemas_parse_errors_stop_reasons_and_checkpoints() {
        let claude = [
            r#"{"type":"custom-title","customTitle":"t","sessionId":"cl-real-1"}"#,
            r#"{"type":"system","subtype":"api_error","level":"error","error":{"status":429,"message":"rate limit"},"retryAttempt":1,"maxRetries":3,"timestamp":"2026-11-28T10:00:01Z","uuid":"s1"}"#,
            r#"{"type":"assistant","uuid":"a2","sessionId":"cl-real-1","timestamp":"2026-11-28T10:00:02Z","thinkingDurationMs":120,"message":{"id":"msg_1","model":"claude-sonnet-4","stop_reason":"end_turn","content":[{"type":"tool_use","id":"tu_inflight","name":"Read","input":{"path":"Cargo.toml"}}],"usage":{"input_tokens":50,"output_tokens":12}}}"#,
        ];
        assert_eq!(
            sniff_session_format(&claude),
            Some(SessionFileFormat::ClaudeJsonl)
        );
        let claude_events =
            parse_session_lines(SessionFileFormat::ClaudeJsonl, &claude, Some("t1"), None);
        assert!(claude_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::ErrorObserved { message } if message.contains("status=429") && message.contains("retry 1/3")
        )));
        assert!(claude_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::TurnCompleted {
                turn_id: Some(id),
                duration_ms: Some(120),
                error: None,
            } if id == "msg_1"
        )));

        let codex = [
            r#"{"ordinal":0,"timestamp":"2026-11-28T10:00:00Z","type":"session_meta","payload":{"session_id":"cx-real","runtime_workspace_roots":["/ws/codex"]}}"#,
            r#"{"ordinal":1,"timestamp":"2026-11-28T10:00:01Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":1000,"completed_at_ms":1250,"item":{"id":"cmd_1","type":"command_execution","status":"completed","exit_code":0,"command":"cargo test"}}}"#,
            r#"{"ordinal":2,"timestamp":"2026-11-28T10:00:02Z","type":"event_msg","payload":{"type":"item_completed","item":{"id":"msg_2","type":"agent_message","content":[{"type":"output_text","text":"all green"}]}}}"#,
            r#"{"ordinal":3,"timestamp":"2026-11-28T10:00:03Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t_cx","duration_ms":900,"error":{"message":"upstream timeout","codex_error_info":"timeout"}}}"#,
        ];
        let cm = sniff_session_metadata(&codex).expect("codex meta");
        assert_eq!(cm.cwd.as_deref(), Some("/ws/codex"));
        let codex_events = parse_session_lines(
            SessionFileFormat::CodexRolloutJsonl,
            &codex,
            Some("t1"),
            None,
        );
        assert!(codex_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::ToolCompleted {
                tool_name,
                ok: true,
                duration_ms: Some(250),
                ..
            } if tool_name == "Bash"
        )));
        assert!(codex_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::AssistantMessage { summary } if summary == "all green"
        )));
        assert!(codex_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::TurnCompleted {
                error: Some(err),
                ..
            } if err == "upstream timeout"
        )));

        let agy = [
            r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-28T10:00:00Z","content":"run"}"#,
            r#"{"step_index":1,"type":"GENERIC","source":"MODEL","status":"DONE","created_at":"2026-11-28T10:00:01Z","content":"checking files"}"#,
            r#"{"step_index":2,"type":"GENERIC","source":"RUN_COMMAND","status":"DONE","created_at":"2026-11-28T10:00:02Z","content":"exit 0"}"#,
            r#"{"step_index":3,"type":"CHECKPOINT","created_at":"2026-11-28T10:00:03Z","input_tokens":100,"output_tokens":20,"cache_read_tokens":40}"#,
        ];
        let agy_events =
            parse_session_lines(SessionFileFormat::AgyFullJsonl, &agy, Some("t1"), None);
        assert!(agy_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::AssistantMessage { summary } if summary == "checking files"
        )));
        assert!(agy_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::ToolCompleted { tool_name, .. } if tool_name == "RUN_COMMAND"
        )));
        assert!(agy_events.iter().any(|e| matches!(
            &e.kind,
            FusedEventKind::UsageRecorded {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 40,
                ..
            }
        )));
    }

    fn sample_observability_events() -> Vec<FusedAgentEvent> {
        let lines = [
            r#"{"type":"assistant","uuid":"a1","sessionId":"s1","timestamp":"2026-11-28T10:00:00Z","message":{"model":"claude-4","content":[{"type":"tool_use","id":"t_done","name":"Read","input":{}},{"type":"tool_use","id":"t_open","name":"Bash","input":{"cmd":"sleep"}}],"usage":{"input_tokens":40,"output_tokens":10}}}"#,
            r#"{"type":"user","uuid":"u1","sessionId":"s1","timestamp":"2026-11-28T10:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t_done","content":"ok"}]}}"#,
        ];
        parse_session_lines(SessionFileFormat::ClaudeJsonl, &lines, Some("t1"), None)
    }

    #[test]
    fn observability_summary_tracks_inflight_usage_and_health() {
        let events = sample_observability_events();
        let status = SessionDiscoveryStatus::Attached {
            method: SessionDiscoveryMethod::ProcessOpenFd,
            confidence: DiscoveryConfidence::ExactFd,
        };

        let busy = summarize_agent_observability(&SummaryInput {
            events: &events,
            is_live: true,
            agent_status: crate::port::AgentStatus::Working,
            active_rule: None,
            has_osc_signal: true,
            process_tree: None,
            discovery_status: status.clone(),
            now_ms: events
                .last()
                .and_then(|e| e.timestamp_ms)
                .map(|t| t + 1_000),
        });
        assert_eq!(busy.health, AgentHealth::Busy);
        assert_eq!(busy.in_flight_tools.len(), 1);
        assert_eq!(busy.in_flight_tools[0].tool_name, "Bash");
        assert_eq!(busy.usage_totals.input_tokens, 40);
        assert_eq!(busy.usage_totals.output_tokens, 10);
        assert!(
            busy.sources_observed
                .contains(&ObservabilitySource::NativeSession)
        );
        assert!(
            busy.sources_observed
                .contains(&ObservabilitySource::Osc7501)
        );

        let stalled = summarize_agent_observability(&SummaryInput {
            events: &events,
            is_live: true,
            agent_status: crate::port::AgentStatus::Working,
            active_rule: None,
            has_osc_signal: false,
            process_tree: None,
            discovery_status: status.clone(),
            now_ms: events
                .last()
                .and_then(|e| e.timestamp_ms)
                .map(|t| t + STALL_THRESHOLD_MS + 500),
        });
        assert!(matches!(stalled.health, AgentHealth::Stalled { .. }));

        let blocked = summarize_agent_observability(&SummaryInput {
            events: &events,
            is_live: true,
            agent_status: crate::port::AgentStatus::Blocked,
            active_rule: Some("permission_prompt"),
            has_osc_signal: false,
            process_tree: None,
            discovery_status: status,
            now_ms: None,
        });
        assert_eq!(
            blocked.health,
            AgentHealth::Blocked {
                rule: "permission_prompt".into()
            }
        );

        let degraded = summarize_agent_observability(&SummaryInput {
            events: &[],
            is_live: true,
            agent_status: crate::port::AgentStatus::Idle,
            active_rule: None,
            has_osc_signal: false,
            process_tree: None,
            discovery_status: SessionDiscoveryStatus::ReadFailed {
                reason: "io err".into(),
            },
            now_ms: None,
        });
        assert_eq!(
            degraded.health,
            AgentHealth::Degraded {
                reason: "io err".into()
            }
        );

        let exited = summarize_agent_observability(&SummaryInput {
            events: &events,
            is_live: false,
            agent_status: crate::port::AgentStatus::Unknown,
            active_rule: None,
            has_osc_signal: false,
            process_tree: None,
            discovery_status: SessionDiscoveryStatus::NoLivePid,
            now_ms: None,
        });
        assert_eq!(exited.health, AgentHealth::Exited);
    }

    #[test]
    fn incremental_cursor_slices_delta_and_signals_reset() {
        let events = sample_observability_events();
        let mid_key = events[1].canonical_key.clone();
        let (delta, latest, _, reset) = slice_events_after_key(events.clone(), Some(&mid_key), 50);
        assert!(!reset);
        assert_eq!(
            latest.as_deref(),
            Some(events.last().unwrap().canonical_key.as_str())
        );
        assert_eq!(delta.len(), events.len() - 2);

        let (empty, _, _, reset_at_tip) =
            slice_events_after_key(events.clone(), latest.as_deref(), 50);
        assert!(!reset_at_tip);
        assert!(empty.is_empty());

        let (reset_slice, _, _, cursor_reset) =
            slice_events_after_key(events, Some("nonexistent-cursor"), 2);
        assert!(cursor_reset);
        assert_eq!(reset_slice.len(), 2);
    }

    fn make_screen_state_event(digest: u64, rule: &str) -> FusedAgentEvent {
        FusedAgentEvent {
            canonical_key: CanonicalEventKey::ScreenSignal {
                digest,
                rule: rule.to_string(),
            }
            .canonical_string(),
            timestamp_ms: None,
            fidelity: EventFidelity::ScreenOnly,
            origins: BTreeSet::from([EventOrigin::HerdrPty]),
            kind: FusedEventKind::ProcessState {
                status: "working".to_string(),
                rule: Some(rule.to_string()),
            },
            session_id: None,
            terminal_address: Some("t1".to_string()),
        }
    }

    #[test]
    fn incremental_cursor_handles_unchanged_screen_and_spinner_churn() {
        let line1 = [
            r#"{"ordinal":0,"timestamp":"2026-11-28T10:00:00Z","type":"session_meta","payload":{"id":"s1","cwd":"/ws"}}"#,
        ];
        let rec1 = parse_session_lines(
            SessionFileFormat::CodexRolloutJsonl,
            &line1,
            Some("t1"),
            None,
        );
        let init_fused = fuse_all_agent_events(
            vec![make_screen_state_event(7, "state:working")],
            rec1.clone(),
            Vec::new(),
        );
        let (init_slice, cur1, _, init_reset) = slice_events_after_key(init_fused, None, 50);
        assert!(!init_reset);
        assert_eq!(cur1.as_deref(), Some(rec1[0].canonical_key.as_str()));
        assert_eq!(init_slice.len(), 2);

        // (a) Screen digest unchanged (=7), new session event arrives -> delivered strictly, no loss.
        let both_lines = [
            line1[0],
            r#"{"ordinal":1,"timestamp":"2026-11-28T10:00:05Z","type":"response_item","payload":{"type":"function_call","call_id":"call_a","name":"Bash","arguments":{}}}"#,
        ];
        let recs2 = parse_session_lines(
            SessionFileFormat::CodexRolloutJsonl,
            &both_lines,
            Some("t1"),
            None,
        );
        let fused2 = fuse_all_agent_events(
            vec![make_screen_state_event(7, "state:working")],
            recs2.clone(),
            Vec::new(),
        );
        let (delta_a, cur2, _, reset_a) = slice_events_after_key(fused2, cur1.as_deref(), 50);
        assert!(!reset_a);
        assert_eq!(delta_a.len(), 1);
        assert_eq!(delta_a[0].canonical_key, recs2[1].canonical_key);
        assert_eq!(cur2.as_deref(), Some(recs2[1].canonical_key.as_str()));

        // (b) Spinner churn changes screen digest (7 -> 8) with no new session events -> empty delta, no reset.
        let fused_spin = fuse_all_agent_events(
            vec![make_screen_state_event(8, "state:working")],
            recs2,
            Vec::new(),
        );
        let (delta_b, cur3, _, reset_b) = slice_events_after_key(fused_spin, cur2.as_deref(), 50);
        assert!(!reset_b);
        assert!(delta_b.is_empty());
        assert_eq!(cur3, cur2);

        // (c) Pure screen-only terminal yields `latest_key = None`.
        let screen_only = fuse_all_agent_events(
            vec![make_screen_state_event(9, "state:idle")],
            Vec::new(),
            Vec::new(),
        );
        let (_, cur_none, _, _) = slice_events_after_key(screen_only, None, 50);
        assert!(cur_none.is_none());
    }

    #[test]
    fn claude_split_assistant_message_deduplicates_usage_by_message_id() {
        let lines = [
            r#"{"type":"assistant","uuid":"u1","sessionId":"s1","timestamp":"2026-11-28T10:00:00Z","message":{"id":"msg_split_1","model":"claude-sonnet-4","content":[{"type":"thinking","thinking":"step 1"}],"usage":{"input_tokens":1000,"output_tokens":5,"cache_read_input_tokens":500,"cache_creation_input_tokens":200}}}"#,
            r#"{"type":"assistant","uuid":"u2","sessionId":"s1","timestamp":"2026-11-28T10:00:01Z","message":{"id":"msg_split_1","model":"claude-sonnet-4","content":[{"type":"tool_use","id":"tu_1","name":"Bash","input":{"cmd":"ls"}}],"usage":{"input_tokens":1000,"output_tokens":40,"cache_read_input_tokens":500,"cache_creation_input_tokens":200}}}"#,
            r#"{"type":"assistant","uuid":"u3","sessionId":"s1","timestamp":"2026-11-28T10:00:02Z","message":{"id":"msg_split_1","model":"claude-sonnet-4","stop_reason":"end_turn","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":1000,"output_tokens":85,"cache_read_input_tokens":500,"cache_creation_input_tokens":200}}}"#,
            r#"{"type":"assistant","uuid":"u4","sessionId":"s1","timestamp":"2026-11-28T10:00:03Z","message":{"model":"claude-sonnet-4","content":[{"type":"text","text":"fallback"}],"usage":{"input_tokens":100,"output_tokens":10}}}"#,
        ];
        let parsed = parse_session_lines(SessionFileFormat::ClaudeJsonl, &lines, Some("t1"), None);
        let usage_events: Vec<_> = parsed
            .iter()
            .filter(|e| matches!(e.kind, FusedEventKind::UsageRecorded { .. }))
            .collect();
        assert_eq!(usage_events.len(), 2);

        let fused = fuse_all_agent_events(Vec::new(), parsed, Vec::new());
        let summary = summarize_agent_observability(&SummaryInput {
            events: &fused,
            is_live: true,
            agent_status: crate::port::AgentStatus::Idle,
            active_rule: None,
            has_osc_signal: false,
            process_tree: None,
            discovery_status: SessionDiscoveryStatus::NoLivePid,
            now_ms: None,
        });
        assert_eq!(summary.usage_totals.input_tokens, 1100);
        assert_eq!(summary.usage_totals.output_tokens, 95);
        assert_eq!(summary.usage_totals.cache_read_tokens, 500);
        assert_eq!(summary.usage_totals.cache_write_tokens, 200);

        // Poll 1 sees u1 only; Poll 2 sees u1..=u3 where message.usage merges forward.
        // Earliest-timestamp anchoring ensures u2 ToolStarted and u3 TurnCompleted are not skipped.
        let parsed1 = parse_session_lines(
            SessionFileFormat::ClaudeJsonl,
            &lines[..1],
            Some("t1"),
            None,
        );
        let fused1 = fuse_all_agent_events(Vec::new(), parsed1, Vec::new());
        let (_, cur1, _, _) = slice_events_after_key(fused1, None, 50);
        assert!(cur1.is_some());

        let parsed3 = parse_session_lines(
            SessionFileFormat::ClaudeJsonl,
            &lines[..3],
            Some("t1"),
            None,
        );
        let fused3 = fuse_all_agent_events(Vec::new(), parsed3, Vec::new());
        let (delta, _, _, reset) = slice_events_after_key(fused3, cur1.as_deref(), 50);
        assert!(!reset);
        assert!(delta.iter().any(|ev| matches!(
            &ev.kind,
            FusedEventKind::ToolStarted { tool_name, .. } if tool_name == "Bash"
        )));
        assert!(
            delta
                .iter()
                .any(|ev| matches!(ev.kind, FusedEventKind::TurnCompleted { .. }))
        );
    }

    #[test]
    fn codex_rollout_keys_remain_stable_across_tail_window_shift() {
        let full = [
            r#"{"timestamp":"2026-11-28T10:00:00Z","type":"session_meta","payload":{"id":"cx-no-ord","cwd":"/ws"}}"#,
            r#"{"timestamp":"2026-11-28T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"checking"}}"#,
            r#"{"timestamp":"2026-11-28T10:00:02Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":200}}"#,
        ];
        let shifted = &full[1..];
        let ev_full = parse_session_lines(
            SessionFileFormat::CodexRolloutJsonl,
            &full,
            Some("t1"),
            None,
        );
        let ev_shifted = parse_session_lines(
            SessionFileFormat::CodexRolloutJsonl,
            shifted,
            Some("t1"),
            None,
        );
        assert_eq!(ev_full.len(), 3);
        assert_eq!(ev_shifted.len(), 2);
        assert_eq!(ev_full[1].canonical_key, ev_shifted[0].canonical_key);
        assert_eq!(ev_full[2].canonical_key, ev_shifted[1].canonical_key);
        assert!(!ev_full[1].canonical_key.contains(":l"));
    }

    #[test]
    fn timeless_cost_state_never_hijacks_incremental_cursor() {
        let m1 = r#"{"type":"assistant","uuid":"u_m1","sessionId":"s1","timestamp":"2026-11-28T10:00:00Z","message":{"id":"msg_1","model":"claude-sonnet-4","content":[{"type":"text","text":"step one"}],"usage":{"input_tokens":100,"output_tokens":10}}}"#;
        let cost = r#"{"type":"cost-state","sessionId":"s1","modelUsage":{"claude-sonnet-4":{"inputTokens":100,"outputTokens":10}}}"#;
        let m2 = r#"{"type":"assistant","uuid":"u_m2","sessionId":"s1","timestamp":"2026-11-28T10:00:05Z","message":{"id":"msg_2","model":"claude-sonnet-4","content":[{"type":"text","text":"running tool"},{"type":"tool_use","id":"tu_m2","name":"Bash","input":{"cmd":"ls"}}],"usage":{"input_tokens":50,"output_tokens":5}}}"#;

        let parsed1 = parse_session_lines(
            SessionFileFormat::ClaudeJsonl,
            &[m1, cost],
            Some("t1"),
            None,
        );
        let fused1 = fuse_all_agent_events(Vec::new(), parsed1, Vec::new());
        let (_, cur1, _, init_reset) = slice_events_after_key(fused1, None, 50);
        assert!(!init_reset);
        let c1 = cur1.as_deref().expect("timestamped anchor cursor");
        assert!(!c1.contains("cost-state"), "unexpected cursor: {c1}");

        let parsed2 = parse_session_lines(
            SessionFileFormat::ClaudeJsonl,
            &[m1, cost, m2],
            Some("t1"),
            None,
        );
        let fused2 = fuse_all_agent_events(Vec::new(), parsed2, Vec::new());
        let (delta, cur2, _, reset) = slice_events_after_key(fused2, Some(c1), 50);
        assert!(!reset);
        assert!(delta.iter().any(|ev| matches!(
            &ev.kind,
            FusedEventKind::AssistantMessage { summary } if summary == "running tool"
        )));
        assert!(delta.iter().any(|ev| matches!(
            &ev.kind,
            FusedEventKind::ToolStarted { tool_call_id, tool_name, .. }
                if tool_call_id.as_deref() == Some("tu_m2") && tool_name == "Bash"
        )));
        let c2 = cur2.as_deref().expect("second anchor cursor");
        assert_ne!(c2, c1);
        assert!(!c2.contains("cost-state"), "unexpected cursor: {c2}");
    }

    #[test]
    fn claude_cost_state_cumulative_snapshots_never_double_count_usage() {
        let m1 = r#"{"type":"assistant","uuid":"u1","sessionId":"s1","timestamp":"2026-11-28T10:00:00Z","message":{"id":"msg_1","model":"claude-sonnet-4","content":[{"type":"text","text":"one"}],"usage":{"input_tokens":100,"output_tokens":10}}}"#;
        let cost1 = r#"{"type":"cost-state","sessionId":"s1","modelUsage":{"claude-sonnet-4":{"inputTokens":100,"outputTokens":10}}}"#;
        let m2 = r#"{"type":"assistant","uuid":"u2","sessionId":"s1","timestamp":"2026-11-28T10:00:05Z","message":{"id":"msg_2","model":"claude-sonnet-4","content":[{"type":"text","text":"two"}],"usage":{"input_tokens":50,"output_tokens":5}}}"#;
        let cost2 = r#"{"type":"cost-state","sessionId":"s1","modelUsage":{"claude-sonnet-4":{"inputTokens":150,"outputTokens":15}}}"#;
        let cost_hi = r#"{"type":"cost-state","sessionId":"s1","modelUsage":{"claude-sonnet-4":{"inputTokens":220,"outputTokens":30}}}"#;

        let sum_totals = |lines: &[&str]| {
            let parsed =
                parse_session_lines(SessionFileFormat::ClaudeJsonl, lines, Some("t1"), None);
            let fused = fuse_all_agent_events(Vec::new(), parsed, Vec::new());
            summarize_agent_observability(&SummaryInput {
                events: &fused,
                is_live: true,
                agent_status: crate::port::AgentStatus::Idle,
                active_rule: None,
                has_osc_signal: false,
                process_tree: None,
                discovery_status: SessionDiscoveryStatus::NoLivePid,
                now_ms: None,
            })
            .usage_totals
        };

        // Case A: interleaved per-message usage and cumulative cost-state snapshots.
        let interleaved = sum_totals(&[m1, cost1, m2, cost2]);
        assert_eq!(interleaved.input_tokens, 150);
        assert_eq!(interleaved.output_tokens, 15);

        // Case B: mid-session cumulative snapshot followed by incremental message.
        let mid_session = sum_totals(&[m1, cost1, m2]);
        assert_eq!(mid_session.input_tokens, 150);
        assert_eq!(mid_session.output_tokens, 15);

        // Case C: pure multiple cost-state snapshots take max cumulative snapshot, never sum.
        let snapshots_only = sum_totals(&[cost1, cost_hi]);
        assert_eq!(snapshots_only.input_tokens, 220);
        assert_eq!(snapshots_only.output_tokens, 30);
    }
}
