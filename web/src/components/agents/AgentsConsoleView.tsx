import {
  Activity,
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  Clock,
  CornerDownLeft,
  Cpu,
  Lock,
  OctagonX,
  RefreshCw,
  Search,
  Send,
  ShieldAlert,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  agentSnapshotUrl,
  buildSendKeysAction,
  buildSendTextAction,
  DEFAULT_AGENT_EVENTS_LIMIT,
  defaultSendPolicyMode,
  describeAgentHealth,
  describeFusedEvent,
  describeSessionDiscovery,
  evaluateActionCapabilities,
  eventsDataFromState,
  formatRssKib,
  formatTokenCount,
  isGuardedPolicyMode,
  isValidScreenGuard,
  parseKeyChordList,
  parseStyledLine,
  reduceAction,
  reduceAgentEvents,
  reduceDiscovery,
  reduceSnapshot,
  snapshotFromState,
  visibleAgentsForRole,
  type ActionSubmissionState,
  type AgentDiscoveryState,
  type AgentEventsLoadState,
  type AgentStatus,
  type AnsiColorTone,
  type DiscoveredTerminal,
  type SendPolicyMode,
  type SnapshotFidelity,
  type SnapshotLoadState,
  type StyledSpan,
  type TerminalLiveState,
  type TuiAction,
  type TuiActionOutcome,
  type TuiSnapshot,
} from "../../agentRuntime.ts";
import {
  fetchGlobalAgentEvents,
  fetchGlobalAgentSnapshot,
  fetchGlobalAgents,
  postGlobalAgentAction,
} from "../../agentRuntimeApi";
import { relativeTime } from "../../format";
import { pageHidden } from "../../validated";
import type { SessionRole } from "../../token.ts";
import { Button, Card, CardHeader, EmptyState, FOCUS_RING, FOCUS_RING_INSET, PageHeader, chip, cx, tr, useLocale } from "../ui";
import { Badge, type BadgeTone } from "../ui/badge";

const DISCOVERY_POLL_MS = 3_000;
const SNAPSHOT_POLL_MS = 2_000;
const EVENTS_POLL_MS = 2_000;
const QUICK_CHORDS = [
  "up",
  "down",
  "left",
  "right",
  "enter",
  "esc",
  "tab",
  "shift+tab",
  "y",
  "n",
  "1",
  "2",
  "3",
  "4",
] as const;

const SEND_POLICY_OPTIONS: ReadonlyArray<{
  mode: SendPolicyMode;
  label: string;
  hint: string;
}> = [
  {
    mode: "only_if_guard",
    label: "仅画面未变 (only_if)",
    hint: "附带当前 ScreenGuard，若屏幕变化返回 409 冲突",
  },
  {
    mode: "only_if_guard_idle",
    label: "画面未变 + Idle",
    hint: "同时校验 ScreenGuard 与 agent_status == idle",
  },
  {
    mode: "only_if_guard_working",
    label: "画面未变 + Working",
    hint: "同时校验 ScreenGuard 与 agent_status == working",
  },
  {
    mode: "when_unblocked",
    label: "非阻塞时发送",
    hint: "agent_status != blocked 时投递（PlainTextOnly 可用）",
  },
  {
    mode: "only_if_idle",
    label: "仅 Idle 时发送",
    hint: "要求 agent_status == idle（不校验选区 guard）",
  },
  {
    mode: "only_if_working",
    label: "仅 Working 时发送",
    hint: "要求 agent_status == working（不校验选区 guard）",
  },
  {
    mode: "force_even_if_blocked",
    label: "强制发送 (force)",
    hint: "即使处于 blocked 状态仍强制注入并回车",
  },
];

type OriginFilter = "all" | "spawn_managed" | "external";

interface AgentsConsoleViewProps {
  role: SessionRole;
  selectedAddress: string | null;
  onSelectAddress: (address: string | null) => void;
}

function agentStatusBadge(status: AgentStatus): { label: string; tone: BadgeTone } {
  switch (status) {
    case "working":
      return { label: "working", tone: "info" };
    case "blocked":
      return { label: "blocked", tone: "warning" };
    case "idle":
      return { label: "idle", tone: "success" };
    case "unknown":
      return { label: "unknown", tone: "neutral" };
  }
}

function liveStateLabel(liveState: TerminalLiveState): {
  label: string;
  tone: BadgeTone;
  detail: string;
} {
  if (liveState.state === "live") {
    return {
      label: liveState.pid !== null ? `Live · PID ${liveState.pid}` : "Live",
      tone: "success",
      detail: liveState.pid !== null ? `pid=${liveState.pid}` : "running",
    };
  }
  const cause = liveState.cause;
  if (cause.kind === "code") {
    return { label: `Exited (${cause.code})`, tone: "danger", detail: `exit code ${cause.code}` };
  }
  if (cause.kind === "signal") {
    return { label: `Signal ${cause.signal}`, tone: "danger", detail: `signal ${cause.signal}` };
  }
  return {
    label: cause.reason ? `Lost (${cause.reason})` : "Exited",
    tone: "neutral",
    detail: cause.reason ?? "exited",
  };
}

function readModeText(fidelity: Extract<SnapshotFidelity, { fidelity: "plain_text_only" }>): string {
  switch (fidelity.read_mode.mode) {
    case "screen":
      return "screen";
    case "history":
      return "history";
    case "stitched":
      return "stitched";
    case "other":
      return `other:${fidelity.read_mode.detail}`;
  }
}

function spanToneClass(tone: AnsiColorTone | null, isBg: boolean): string {
  if (!tone) return "";
  if (isBg) {
    switch (tone) {
      case "danger":
        return "bg-fail-tint";
      case "success":
        return "bg-ok-tint";
      case "warning":
        return "bg-wait-tint";
      case "info":
      case "accent":
        return "bg-info-tint";
      case "purple":
        return "bg-judge-tint";
      case "muted":
      case "bright":
        return "bg-white/10";
    }
  }
  switch (tone) {
    case "danger":
      return "text-destructive";
    case "success":
      return "text-success";
    case "warning":
      return "text-warning";
    case "info":
    case "accent":
      return "text-info";
    case "purple":
      return "text-jev";
    case "muted":
      return "text-faint-foreground";
    case "bright":
      return "text-foreground";
  }
}

function spanClassName(span: StyledSpan): string {
  if (span.inverse) {
    return cx(
      "rounded-[2px] bg-white px-0.5 font-semibold text-black",
      span.underline && "underline",
    );
  }
  return cx(
    span.bold && "font-semibold",
    span.dim && "opacity-70",
    span.underline && "underline",
    spanToneClass(span.fg, false),
    spanToneClass(span.bg, true),
  );
}

function formatActionOutcome(outcome: TuiActionOutcome): string {
  switch (outcome.outcome_kind) {
    case "text_sent":
      return `已发送文本：turn=${outcome.turn}${outcome.status ? ` · status=${outcome.status}` : ""}${outcome.started === true ? " · 新回合已启动" : ""}`;
    case "keys_sent":
      return `已发送按键序列：turn=${outcome.turn}${outcome.status ? ` · status=${outcome.status}` : ""}`;
    case "interrupted":
      return `已发送中断（${outcome.key_sent ?? "default"}）：outcome=${outcome.outcome.outcome === "other" ? outcome.outcome.raw : outcome.outcome.outcome}`;
    case "waited": {
      const reason =
        outcome.reason.kind === "other" ? outcome.reason.raw : outcome.reason.kind;
      return `等待结束（${outcome.terminal}）：reason=${reason} · turn=${outcome.turn}`;
    }
  }
}

function describePolicy(mode: SendPolicyMode): string {
  return SEND_POLICY_OPTIONS.find((opt) => opt.mode === mode)?.hint ?? "";
}

function matchesSearch(agent: DiscoveredTerminal, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  const hay = [
    agent.address,
    agent.name ?? "",
    agent.title,
    agent.machine ?? "",
    agent.agent ?? "",
    agent.cwd,
    agent.telemetry?.active_command ?? "",
    agent.session?.native_session_id ?? "",
    agent.origin.kind === "spawn_managed" ? agent.origin.owner_session : "external",
  ]
    .join(" ")
    .toLowerCase();
  return hay.includes(needle);
}

export function AgentsConsoleView({
  role,
  selectedAddress,
  onSelectAddress,
}: AgentsConsoleViewProps) {
  const [includeAll, setIncludeAll] = useState(false);
  const [originFilter, setOriginFilter] = useState<OriginFilter>("all");
  const [query, setQuery] = useState("");
  const [refreshSeq, setRefreshSeq] = useState(0);
  const [mobileDetailOpen, setMobileDetailOpen] = useState(() => selectedAddress !== null);
  const [serverReadOnlyRefused, setServerReadOnlyRefused] = useState(false);

  const isReadOnly = role.kind === "viewer" || serverReadOnlyRefused;

  const { locale } = useLocale();
  const [discovery, setDiscovery] = useState<AgentDiscoveryState>(() => ({
    kind: "loading",
    includeAll,
  }));
  const [snapshotState, setSnapshotState] = useState<SnapshotLoadState>(() =>
    selectedAddress
      ? { kind: "loading", address: selectedAddress, previous: null }
      : { kind: "idle" },
  );
  const [eventsState, setEventsState] = useState<AgentEventsLoadState>(() =>
    selectedAddress
      ? { kind: "loading", address: selectedAddress, previous: null }
      : { kind: "idle" },
  );
  const [observabilityTab, setObservabilityTab] = useState<"events" | "procs">("events");
  const [observabilityExpanded, setObservabilityExpanded] = useState(true);
  const [actionState, setActionState] = useState<ActionSubmissionState>({
    kind: "idle",
  });

  // Action composer inputs
  const [textDraft, setTextDraft] = useState("");
  const [policyMode, setPolicyMode] = useState<SendPolicyMode>("only_if_guard");
  const [waitAfterSendMs, setWaitAfterSendMs] = useState<string>("");
  const [chordInput, setChordInput] = useState("");
  const [interruptConfirming, setInterruptConfirming] = useState(false);
  const policyInitAddressRef = useRef<string | null>(null);
  const eventsCursorRef = useRef<{ address: string | null; key: string | null }>({
    address: selectedAddress,
    key: null,
  });
  const selectedRef = useRef<string | null>(selectedAddress);
  selectedRef.current = selectedAddress;

  // Reset role-scoped state cleanly when switching credentials / viewer mode
  useEffect(() => {
    if (role.kind === "viewer") {
      setOriginFilter((curr) => (curr === "external" ? "all" : curr));
    } else {
      setServerReadOnlyRefused(false);
    }
  }, [role]);

  // Reset per-terminal transient composer banners/confirmations on address switch
  useEffect(() => {
    setActionState((curr) =>
      curr.kind === "idle" || ("address" in curr && curr.address === selectedAddress)
        ? curr
        : { kind: "idle" },
    );
    setInterruptConfirming(false);
    if (selectedAddress === null) {
      setMobileDetailOpen(false);
      setSnapshotState({ kind: "idle" });
      setEventsState({ kind: "idle" });
      eventsCursorRef.current = { address: null, key: null };
      policyInitAddressRef.current = null;
    } else {
      if (eventsCursorRef.current.address !== selectedAddress) {
        eventsCursorRef.current = { address: selectedAddress, key: null };
      }
      setSnapshotState((curr) => reduceSnapshot(curr, { type: "selected", address: selectedAddress }));
      setEventsState((curr) => reduceAgentEvents(curr, { type: "selected", address: selectedAddress }));
    }
  }, [selectedAddress]);

  // Auto-reset two-step interrupt confirmation after 4s
  useEffect(() => {
    if (!interruptConfirming) return;
    const timer = window.setTimeout(() => setInterruptConfirming(false), 4_000);
    return () => window.clearTimeout(timer);
  }, [interruptConfirming]);

  // Discovery polling (`GET /api/agents[?all=true]`)
  useEffect(() => {
    let cancelled = false;
    let inFlight = false;
    let controller: AbortController | null = null;
    setDiscovery((curr) =>
      reduceDiscovery(curr, { type: "fetch_started", includeAll }),
    );

    const tick = async () => {
      if (pageHidden(document) || inFlight) return;
      inFlight = true;
      controller = new AbortController();
      try {
        const res = await fetchGlobalAgents(includeAll, controller.signal);
        if (cancelled) return;
        if (res.ok) {
          const visible = visibleAgentsForRole(res.data.agents, role);
          setDiscovery((curr) =>
            reduceDiscovery(curr, {
              type: "fetch_succeeded",
              includeAll,
              agents: visible,
              nowMs: Date.now(),
            }),
          );
        } else {
          setDiscovery((curr) =>
            reduceDiscovery(curr, {
              type: "fetch_failed",
              includeAll,
              error: res.error,
            }),
          );
        }
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
      } finally {
        inFlight = false;
      }
    };

    const onVisibility = () => {
      if (!pageHidden(document)) void tick();
    };
    void tick();
    const timer = window.setInterval(() => void tick(), DISCOVERY_POLL_MS);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      controller?.abort();
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [includeAll, refreshSeq, role]);

  const rawAgents = useMemo<DiscoveredTerminal[]>(() => {
    if (discovery.kind === "ready") return discovery.agents;
    if (discovery.kind === "error" && discovery.previous) return discovery.previous;
    return [];
  }, [discovery]);

  const visibleAgents = useMemo(() => {
    const roleAgents = visibleAgentsForRole(rawAgents, role);
    return roleAgents.filter((agent) => {
      if (originFilter === "spawn_managed" && agent.origin.kind !== "spawn_managed") {
        return false;
      }
      if (originFilter === "external" && agent.origin.kind !== "external") {
        return false;
      }
      return matchesSearch(agent, query);
    });
  }, [rawAgents, role, originFilter, query]);

  // Select first discovered terminal automatically when none is selected
  useEffect(() => {
    if (discovery.kind !== "ready") return;
    if (selectedAddress === null && discovery.agents.length > 0) {
      onSelectAddress(discovery.agents[0].address);
    }
  }, [discovery, selectedAddress, onSelectAddress]);

  const refreshSnapshotOnce = useCallback(
    async (address: string, bypassEtag: boolean, signal: AbortSignal) => {
      const res = await fetchGlobalAgentSnapshot(address, signal, bypassEtag);
      if (signal.aborted || selectedRef.current !== address) return res;
      if (res.ok) {
        setSnapshotState((curr) =>
          reduceSnapshot(curr, {
            type: "fetch_succeeded",
            address,
            snapshot: res.data,
            nowMs: Date.now(),
          }),
        );
        if (policyInitAddressRef.current !== address) {
          policyInitAddressRef.current = address;
          setPolicyMode(defaultSendPolicyMode(res.data));
        }
        setActionState((curr) =>
          reduceAction(curr, {
            type: "conflict_snapshot_refreshed",
            address,
            freshGuard: res.data.guard,
          }),
        );
      } else {
        setSnapshotState((curr) =>
          reduceSnapshot(curr, {
            type: "fetch_failed",
            address,
            error: res.error,
          }),
        );
        setActionState((curr) =>
          reduceAction(curr, {
            type: "conflict_snapshot_refresh_failed",
            address,
          }),
        );
      }
      return res;
    },
    [],
  );

  const refreshEventsOnce = useCallback(
    async (address: string, forceFull: boolean, signal: AbortSignal) => {
      const requestedAfterKey =
        forceFull || eventsCursorRef.current.address !== address
          ? null
          : eventsCursorRef.current.key;
      const res = await fetchGlobalAgentEvents(address, {
        limit: DEFAULT_AGENT_EVENTS_LIMIT,
        afterKey: requestedAfterKey,
        bypassEtag: forceFull,
        signal,
      });
      if (signal.aborted || selectedRef.current !== address) return res;
      if (res.ok) {
        const nextCursorKey =
          res.data.latest_key !== null
            ? res.data.latest_key
            : res.data.cursor_reset || requestedAfterKey === null
              ? null
              : requestedAfterKey;
        eventsCursorRef.current = { address, key: nextCursorKey };
        setEventsState((curr) =>
          reduceAgentEvents(curr, {
            type: "fetch_succeeded",
            address,
            requestedAfterKey,
            report: res.data,
            nowMs: Date.now(),
          }),
        );
      } else {
        setEventsState((curr) =>
          reduceAgentEvents(curr, {
            type: "fetch_failed",
            address,
            error: res.error,
          }),
        );
      }
      return res;
    },
    [],
  );

  // Selected terminal snapshot polling (`GET /api/agents/<addr>/snapshot`)
  useEffect(() => {
    if (!selectedAddress) return;
    let cancelled = false;
    let inFlight = false;
    let controller: AbortController | null = null;

    const tick = async () => {
      if (pageHidden(document) || inFlight) return;
      inFlight = true;
      controller = new AbortController();
      try {
        await refreshSnapshotOnce(selectedAddress, false, controller.signal);
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
      } finally {
        if (!cancelled) inFlight = false;
      }
    };

    const onVisibility = () => {
      if (!pageHidden(document)) void tick();
    };
    void tick();
    const timer = window.setInterval(() => void tick(), SNAPSHOT_POLL_MS);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      controller?.abort();
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [selectedAddress, refreshSeq, refreshSnapshotOnce]);

  // Incremental observability events & process tree polling (`GET /api/agents/<addr>/events?after_key=`)
  useEffect(() => {
    if (!selectedAddress) return;
    let cancelled = false;
    let inFlight = false;
    let firstPoll = true;
    let controller: AbortController | null = null;

    const tick = async () => {
      if (pageHidden(document) || inFlight) return;
      inFlight = true;
      controller = new AbortController();
      const forceFull = firstPoll;
      try {
        await refreshEventsOnce(selectedAddress, forceFull, controller.signal);
        if (!controller.signal.aborted) {
          firstPoll = false;
        }
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
      } finally {
        if (!cancelled) inFlight = false;
      }
    };

    const onVisibility = () => {
      if (!pageHidden(document)) void tick();
    };
    void tick();
    const timer = window.setInterval(() => void tick(), EVENTS_POLL_MS);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      controller?.abort();
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [selectedAddress, refreshSeq, refreshEventsOnce]);

  const scopedSnapshotState: SnapshotLoadState = useMemo(() => {
    if (selectedAddress === null) return { kind: "idle" };
    if (snapshotState.kind !== "idle" && snapshotState.address === selectedAddress) {
      return snapshotState;
    }
    return { kind: "loading", address: selectedAddress, previous: null };
  }, [snapshotState, selectedAddress]);

  const eventsData = useMemo(
    () => eventsDataFromState(eventsState, selectedAddress),
    [eventsState, selectedAddress],
  );
  const healthMeta = useMemo(
    () => (eventsData ? describeAgentHealth(eventsData.summary.health, locale) : null),
    [eventsData, locale],
  );

  const visibleActionState: ActionSubmissionState =
    actionState.kind === "idle" ||
    (selectedAddress !== null && actionState.address === selectedAddress)
      ? actionState
      : { kind: "idle" };

  const activeSnapshot: TuiSnapshot | null = snapshotFromState(
    snapshotState,
    selectedAddress,
  );
  const parsedStyledLines = useMemo(
    () =>
      activeSnapshot && activeSnapshot.fidelity.fidelity === "styled_vt"
        ? activeSnapshot.fidelity.styled_lines.map((rawLine) => parseStyledLine(rawLine))
        : null,
    [activeSnapshot],
  );
  const selectedSummary = useMemo(
    () => rawAgents.find((a) => a.address === selectedAddress) ?? null,
    [rawAgents, selectedAddress],
  );
  const capabilities = useMemo(
    () =>
      evaluateActionCapabilities(
        role,
        serverReadOnlyRefused,
        snapshotState,
        selectedAddress,
      ),
    [role, serverReadOnlyRefused, snapshotState, selectedAddress],
  );

  const actionSubmitting = actionState.kind === "submitting";

  const dispatchAction = useCallback(
    async (targetAddress: string, action: TuiAction): Promise<boolean> => {
      setActionState({ kind: "submitting", address: targetAddress, action });
      const res = await postGlobalAgentAction(targetAddress, action);
      if (selectedRef.current !== targetAddress) return res.ok;

      if (res.ok) {
        setActionState({
          kind: "succeeded",
          address: targetAddress,
          action,
          outcome: res.data,
          completedAtMs: Date.now(),
        });
        const refreshCtrl = new AbortController();
        void refreshSnapshotOnce(targetAddress, true, refreshCtrl.signal).catch(
          () => undefined,
        );
        void refreshEventsOnce(targetAddress, false, refreshCtrl.signal).catch(
          () => undefined,
        );
        return true;
      }

      if (res.error.kind === "forbidden_readonly") {
        setServerReadOnlyRefused(true);
      }

      setActionState((curr) =>
        reduceAction(curr, {
          type: "submit_failed",
          address: targetAddress,
          action,
          error: res.error,
        }),
      );

      // Fail-closed 409 guard_mismatch UX: immediately refresh latest snapshot (never retry!)
      if (res.error.kind === "guard_mismatch") {
        const refreshCtrl = new AbortController();
        void refreshSnapshotOnce(targetAddress, true, refreshCtrl.signal).catch(
          () => {
            setActionState((curr) =>
              reduceAction(curr, {
                type: "conflict_snapshot_refresh_failed",
                address: targetAddress,
              }),
            );
          },
        );
      }
      return false;
    },
    [refreshSnapshotOnce, refreshEventsOnce],
  );

  const handleSendText = async () => {
    if (!selectedAddress || actionSubmitting) return;
    const waitMsRaw = waitAfterSendMs.trim();
    let waitTimeoutMs: number | undefined;
    if (waitMsRaw.length > 0) {
      const parsed = Number.parseInt(waitMsRaw, 10);
      if (!Number.isFinite(parsed) || parsed < 1 || parsed > 120_000) {
        setActionState({
          kind: "failed",
          address: selectedAddress,
          message: "发送后等待超时需为 1..=120000 的整数毫秒",
        });
        return;
      }
      waitTimeoutMs = parsed;
    }
    const built = buildSendTextAction({
      text: textDraft,
      mode: policyMode,
      waitTimeoutMs,
      capabilities,
    });
    if (!built.ok) {
      setActionState({
        kind: "failed",
        address: selectedAddress,
        message: built.message,
      });
      return;
    }
    const ok = await dispatchAction(selectedAddress, built.action);
    if (ok) {
      setTextDraft("");
    }
  };

  const handleSendQuickChords = async (keys: string[]): Promise<boolean> => {
    if (!selectedAddress || actionSubmitting) return false;
    const built = buildSendKeysAction({
      keys,
      useGuard: true,
      capabilities,
    });
    if (!built.ok) {
      setActionState({
        kind: "failed",
        address: selectedAddress,
        message: built.message,
      });
      return false;
    }
    return dispatchAction(selectedAddress, built.action);
  };

  const handleSendCustomChords = async () => {
    if (!selectedAddress || actionSubmitting) return;
    const parsed = parseKeyChordList(chordInput);
    if (!parsed.ok) {
      setActionState({
        kind: "failed",
        address: selectedAddress,
        message: parsed.message,
      });
      return;
    }
    const ok = await handleSendQuickChords(parsed.keys);
    if (ok) setChordInput("");
  };

  const handleInterrupt = async () => {
    if (!selectedAddress || actionSubmitting || isReadOnly || capabilities.terminalExited) {
      return;
    }
    if (!interruptConfirming) {
      setInterruptConfirming(true);
      return;
    }
    setInterruptConfirming(false);
    await dispatchAction(selectedAddress, {
      action: "interrupt",
      timeout_ms: 5_000,
    });
  };

  const handleWaitSettled = async () => {
    if (!selectedAddress || actionSubmitting || isReadOnly || capabilities.terminalExited) {
      return;
    }
    await dispatchAction(selectedAddress, {
      action: "wait_settled",
      timeout_ms: 5_000,
    });
  };

  const selectedDiscoveryStatus =
    eventsData?.summary.discovery_status ??
    selectedSummary?.telemetry?.discovery_status ??
    null;
  const discoveryStatusMeta = useMemo(
    () =>
      selectedDiscoveryStatus
        ? describeSessionDiscovery(selectedDiscoveryStatus, locale)
        : null,
    [selectedDiscoveryStatus, locale],
  );
  const selectedRssKib =
    eventsData?.process_tree?.total_rss_kib ??
    selectedSummary?.telemetry?.total_rss_kib ??
    null;
  const selectedChildCount =
    eventsData?.process_tree?.child_count ??
    selectedSummary?.telemetry?.child_count ??
    null;
  const selectedProcessCount =
    eventsData?.process_tree?.process_count ??
    selectedSummary?.telemetry?.process_count ??
    null;
  const selectedLeafCommand =
    eventsData?.process_tree?.active_leaf_command ??
    selectedSummary?.telemetry?.active_command ??
    null;
  const showListOnMobile = !mobileDetailOpen || selectedAddress === null;

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <>
            <SquareTerminal size={16} className="shrink-0 text-muted-foreground" />
            <h1 className="truncate text-body font-semibold text-foreground">
              Agents
            </h1>
            <Badge tone="neutral" count={visibleAgents.length} />
            {isReadOnly ? (
              <Badge
                tone="info"
                size="md"
                icon={<Lock size={11} />}
                title={
                  role.kind === "viewer"
                    ? `ReadOnlyViewer token 过期时间：${new Date(role.expiresAtMs).toLocaleString()}`
                    : "服务端拒绝变更请求（只读会话）"
                }
              >
                {tr(locale, "只读观察", "Read-Only")} · ReadOnlyViewer
                {role.kind === "viewer" ? ` (${relativeTime(role.expiresAtMs)})` : ""}
              </Badge>
            ) : (
              <Badge tone="success" size="md">
                {tr(locale, "操作模式", "Interactive")}
              </Badge>
            )}
          </>
        }
      >
        <button
          type="button"
          onClick={() => setIncludeAll((v) => !v)}
          aria-pressed={includeAll}
          className={cx(
            chip(includeAll),
            "pointer-coarse:h-10",
            FOCUS_RING,
          )}
          title={
            isReadOnly
              ? "切换 GET /api/agents?all=true（包含已退出的 Spawn 托管终端）"
              : "切换 GET /api/agents?all=true（包含已退出终端；外部终端可见性由角色权限控制）"
          }
        >
          {includeAll ? tr(locale, "包含已退出 (?all=true)", "Include exited (?all=true)") : tr(locale, "仅运行中", "Live only")}
        </button>
        <Button
          onClick={() => setRefreshSeq((s) => s + 1)}
          className="pointer-coarse:h-10"
          title={tr(locale, "立即刷新全局终端列表与当前屏幕快照", "Refresh agents list and terminal snapshot")}
        >
          <RefreshCw size={13} />
          {tr(locale, "刷新", "Refresh")}
        </Button>
      </PageHeader>

      <div className="flex min-h-0 min-w-0 flex-1 flex-col md:flex-row">
        {/* Left pane: Global Herdr Terminal Discovery */}
        <section
          aria-label="Global Herdr terminals"
          className={cx(
            "min-h-0 flex-col border-surface-border bg-app-shell md:w-72 md:shrink-0 md:border-r lg:w-80",
            showListOnMobile ? "flex flex-1 md:flex-none" : "hidden md:flex",
          )}
        >
          <div className="shrink-0 space-y-2 border-b border-surface-border p-3">
            <div className="relative">
              <Search
                size={13}
                aria-hidden="true"
                className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-faint-foreground"
              />
              <input
                type="search"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="筛选地址、名称、工作目录或引擎…"
                aria-label="筛选发现的全局终端"
                className={cx(
                  "h-9 w-full rounded-control border border-surface-border bg-surface pr-2.5 pl-8 text-label text-foreground placeholder:text-faint-foreground pointer-coarse:h-11",
                  FOCUS_RING_INSET,
                )}
              />
            </div>

            <div className="flex flex-wrap items-center gap-1.5">
              <button
                type="button"
                onClick={() => setOriginFilter("all")}
                className={cx(chip(originFilter === "all"), "pointer-coarse:h-10", FOCUS_RING)}
              >
                全部 ({visibleAgentsForRole(rawAgents, role).length})
              </button>
              <button
                type="button"
                onClick={() => setOriginFilter("spawn_managed")}
                className={cx(
                  chip(originFilter === "spawn_managed"),
                  "pointer-coarse:h-10",
                  FOCUS_RING,
                )}
              >
                Spawn 受管
              </button>
              {role.kind !== "viewer" && (
                <button
                  type="button"
                  onClick={() => setOriginFilter("external")}
                  className={cx(
                    chip(originFilter === "external"),
                    "pointer-coarse:h-10",
                    FOCUS_RING,
                  )}
                >
                  外部终端
                </button>
              )}
            </div>
          </div>

          <div className="min-h-0 flex-1 overflow-y-auto p-2">
            {discovery.kind === "loading" && rawAgents.length === 0 ? (
              <div className="grid h-40 place-items-center text-caption text-faint-foreground">
                正在发现全局 Herdr 终端…
              </div>
            ) : discovery.kind === "unauthorized" ? (
              <div className="rounded-control bg-fail-tint p-3 text-caption text-destructive">
                {discovery.message}
              </div>
            ) : discovery.kind === "forbidden" ? (
              <div className="rounded-control bg-fail-tint p-3 text-caption text-destructive">
                {discovery.message}
              </div>
            ) : (
              <>
                {discovery.kind === "error" && (
                  <div className="mb-2 rounded-control bg-fail-tint px-3 py-2 text-caption text-destructive">
                    {discovery.message}
                  </div>
                )}
                {visibleAgents.length === 0 ? (
                  <EmptyState
                    icon={<SquareTerminal size={18} />}
                    title="未发现匹配的终端"
                    hint={
                      isReadOnly
                        ? includeAll
                          ? "当前无运行中或已退出的 Spawn 受管 Herdr 终端（ReadOnlyViewer 不会显示外部非受管终端）。"
                          : "当前无运行中的 Spawn 受管 Herdr 终端；可切换「包含已退出 (?all=true)」查看历史 Spawn 终端（外部终端仍不曝光）。"
                        : "确认 herdr-core 正在运行，或切换「仅运行中 / 包含已退出 (?all=true)」查看更多终端。"
                    }
                    className="px-3 py-10"
                  />
                ) : (
                  <ul className="space-y-1">
                    {visibleAgents.map((terminal) => {
                      const selected = terminal.address === selectedAddress;
                      const stBadge = agentStatusBadge(terminal.agent_status);
                      const liveMeta = liveStateLabel(terminal.state);
                      return (
                        <li key={terminal.address}>
                          <button
                            type="button"
                            onClick={() => {
                              onSelectAddress(terminal.address);
                              setMobileDetailOpen(true);
                            }}
                            aria-current={selected ? "true" : undefined}
                            className={cx(
                              "flex min-h-11 w-full flex-col gap-1 rounded-control px-2.5 py-2 text-left transition-colors",
                              FOCUS_RING_INSET,
                              selected
                                ? "bg-surface-selected shadow-hairline"
                                : "hover:bg-surface-hover",
                            )}
                          >
                            <div className="flex min-w-0 items-center gap-1.5">
                              <span className="truncate font-mono text-label font-semibold text-foreground">
                                {terminal.address}
                              </span>
                              {terminal.origin.kind === "spawn_managed" ? (
                                <Badge tone="purple">Spawn</Badge>
                              ) : (
                                <Badge tone="neutral">External</Badge>
                              )}
                              <Badge
                                tone={stBadge.tone}
                                dot
                                live={terminal.agent_status === "working"}
                                className="ml-auto"
                              >
                                {stBadge.label}
                              </Badge>
                            </div>
                            <div className="flex min-w-0 items-center gap-1.5 text-caption text-muted-foreground">
                              <span className="min-w-0 flex-1 truncate">
                                {terminal.name ?? (terminal.title || terminal.argv[0] || "terminal")}
                              </span>
                              <span className="tnum shrink-0 font-mono text-micro text-faint-foreground">
                                {terminal.cols}×{terminal.rows} · T{terminal.turn}
                              </span>
                            </div>
                            <div className="flex min-w-0 items-center gap-1.5 text-micro text-faint-foreground">
                              <Badge tone={liveMeta.tone}>{liveMeta.label}</Badge>
                              {terminal.agent && (
                                <span className="shrink-0 font-mono text-muted-foreground">
                                  {terminal.agent}
                                </span>
                              )}
                              <span className="min-w-0 flex-1 truncate font-mono">
                                {terminal.cwd}
                              </span>
                            </div>
                            {terminal.telemetry && (
                              <div className="flex min-w-0 items-center gap-1.5 font-mono text-micro text-faint-foreground">
                                <span className="tnum shrink-0">
                                  {formatRssKib(terminal.telemetry.total_rss_kib)}
                                </span>
                                {terminal.telemetry.child_count > 0 && (
                                  <Badge tone="neutral">+{terminal.telemetry.child_count}</Badge>
                                )}
                                {terminal.session ? (
                                  <Badge tone="purple">
                                    {terminal.session.format.replace(/_(rollout_)?jsonl$/, "")}
                                  </Badge>
                                ) : terminal.telemetry.discovery_status.status === "read_failed" ? (
                                  <Badge tone="warning">
                                    {tr(locale, "会话读取失败", "read_failed")}
                                  </Badge>
                                ) : null}
                                {terminal.telemetry.active_command && (
                                  <span className="min-w-0 flex-1 truncate">
                                    {terminal.telemetry.active_command}
                                  </span>
                                )}
                              </div>
                            )}
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </>
            )}
          </div>
        </section>

        {/* Right pane: Live Snapshot & Guarded TUI Control */}
        <section
          aria-label="Selected terminal console"
          className={cx(
            "min-h-0 min-w-0 flex-1 flex-col overflow-y-auto p-3 sm:p-4",
            !showListOnMobile ? "flex" : "hidden md:flex",
          )}
        >
          {selectedAddress !== null && (
            <div className="mb-2 flex items-center justify-between gap-2 md:hidden">
              <Button
                onClick={() => setMobileDetailOpen(false)}
                className="pointer-coarse:h-11"
              >
                <ArrowLeft size={14} />
                返回终端列表
              </Button>
              <span className="truncate font-mono text-caption text-muted-foreground">
                {selectedAddress}
              </span>
            </div>
          )}

          {selectedAddress === null ? (
            <div className="grid min-h-60 flex-1 place-items-center">
              <EmptyState
                icon={<SquareTerminal size={20} />}
                title="选择一个全局终端"
                hint="从左侧列表选择终端以查看实时屏幕保真度、ScreenGuard 摘要与可控操作。"
              />
            </div>
          ) : scopedSnapshotState.kind === "forbidden_external" ? (
            <Card className="p-5">
              <div className="flex items-start gap-3">
                <ShieldAlert size={18} className="mt-0.5 shrink-0 text-destructive" />
                <div className="space-y-1">
                  <h2 className="text-body font-semibold text-foreground">
                    外部终端观察已拦截（HTTP 403）
                  </h2>
                  <p className="text-caption text-destructive">
                    {scopedSnapshotState.message}
                  </p>
                  <p className="text-caption text-muted-foreground">
                    `ReadOnlyViewer` 凭据仅允许列出和读取 Spawn 受管终端的屏幕快照；未触发任何外部读屏操作。
                  </p>
                </div>
              </div>
            </Card>
          ) : scopedSnapshotState.kind === "unauthorized" ? (
            <Card className="p-5">
              <p className="text-caption text-destructive">
                {scopedSnapshotState.message}
              </p>
            </Card>
          ) : scopedSnapshotState.kind === "not_found" ? (
            <Card className="p-5">
              <EmptyState
                title={`终端 ${scopedSnapshotState.address} 已不存在或已释放`}
                hint={
                  <div className="flex flex-col items-center gap-3">
                    <span>{scopedSnapshotState.message}</span>
                    <Button
                      onClick={() => {
                        onSelectAddress(null);
                        setRefreshSeq((s) => s + 1);
                      }}
                    >
                      清除选择并刷新列表
                    </Button>
                  </div>
                }
              />
            </Card>
          ) : (
            <div className="flex min-h-0 flex-1 flex-col gap-3">
              {/* Snapshot Metadata Card */}
              <Card>
                <CardHeader
                  icon={<SquareTerminal size={14} />}
                  title={
                    <span className="flex flex-wrap items-center gap-2">
                      <span className="font-mono font-semibold">
                        {selectedAddress}
                      </span>
                      {selectedSummary?.origin.kind === "spawn_managed" ? (
                        <Badge tone="purple">
                          Spawn · {selectedSummary.origin.terminal_name}
                        </Badge>
                      ) : selectedSummary?.origin.kind === "external" ? (
                        <Badge tone="neutral">External</Badge>
                      ) : null}
                      {activeSnapshot && (
                        <>
                          <Badge
                            tone={agentStatusBadge(activeSnapshot.agent_status).tone}
                            dot
                            live={activeSnapshot.agent_status === "working"}
                          >
                            {agentStatusBadge(activeSnapshot.agent_status).label}
                          </Badge>
                          <Badge
                            tone={liveStateLabel(activeSnapshot.live_state).tone}
                          >
                            {liveStateLabel(activeSnapshot.live_state).label}
                          </Badge>
                        </>
                      )}
                    </span>
                  }
                  meta={
                    activeSnapshot ? (
                      <span className="tnum flex flex-wrap items-center gap-2 font-mono text-micro">
                        <span>
                          {activeSnapshot.cols}×{activeSnapshot.rows}
                        </span>
                        {selectedSummary && <span>T{selectedSummary.turn}</span>}
                        <span
                          className="rounded bg-inset px-1.5 py-px text-foreground select-all"
                          title={`Endpoint: ${agentSnapshotUrl(selectedAddress)} | guard=${activeSnapshot.guard}`}
                        >
                          guard {activeSnapshot.guard}
                        </span>
                      </span>
                    ) : (
                      <span>加载中…</span>
                    )
                  }
                />

                {activeSnapshot && (
                  <div className="flex flex-wrap items-center gap-2 px-4 py-2.5 text-caption">
                    {activeSnapshot.fidelity.fidelity === "styled_vt" ? (
                      <>
                        <Badge tone="success" size="md" dot>
                          StyledVt
                        </Badge>
                        <span className="font-mono text-muted-foreground">
                          cursor{" "}
                          {activeSnapshot.fidelity.cursor
                            ? `(${activeSnapshot.fidelity.cursor.row}, ${activeSnapshot.fidelity.cursor.col})`
                            : "—"}
                        </span>
                        <span className="text-faint-foreground">·</span>
                        <span className="font-mono text-muted-foreground">
                          alt-screen:{" "}
                          {activeSnapshot.fidelity.alt_screen ? "on" : "off"}
                        </span>
                        <span className="text-faint-foreground">·</span>
                        <span className="text-success">选区可观测</span>
                      </>
                    ) : (
                      <>
                        <Badge tone="warning" size="md" dot>
                          PlainTextOnly
                        </Badge>
                        <span className="font-mono text-muted-foreground">
                          read_mode={readModeText(activeSnapshot.fidelity)}
                        </span>
                        <span className="text-faint-foreground">·</span>
                        <span className="text-muted-foreground">
                          {activeSnapshot.fidelity.complete ? "complete=true" : "truncated"}
                        </span>
                        {activeSnapshot.fidelity.why && (
                          <>
                            <span className="text-faint-foreground">·</span>
                            <span className="font-mono text-warning">
                              why={activeSnapshot.fidelity.why}
                            </span>
                          </>
                        )}
                      </>
                    )}

                    {activeSnapshot.agent && (
                      <Badge tone="neutral">{activeSnapshot.agent}</Badge>
                    )}
                    {activeSnapshot.rule && (
                      <Badge tone="info">rule: {activeSnapshot.rule}</Badge>
                    )}
                    {activeSnapshot.blocker_visible && (
                      <Badge tone="warning" icon={<AlertTriangle size={11} />}>
                        blocker_visible
                      </Badge>
                    )}
                    {activeSnapshot.title && (
                      <span className="ml-auto max-w-full truncate text-micro text-faint-foreground">
                        {activeSnapshot.title}
                      </span>
                    )}
                  </div>
                )}

                {scopedSnapshotState.kind === "error" && (
                  <div className="border-t border-surface-border bg-fail-tint px-4 py-2 text-caption text-destructive">
                    {scopedSnapshotState.message}
                  </div>
                )}

                {(eventsData || selectedSummary?.telemetry || eventsState.kind === "error") && (
                  <div className="space-y-1.5 border-t border-surface-border bg-inset/60 px-4 py-2.5 text-caption">
                    <div className="flex flex-wrap items-center gap-2">
                      {healthMeta && (
                        <Badge
                          tone={healthMeta.tone}
                          size="md"
                          dot
                          live={healthMeta.live}
                          title={healthMeta.detail ?? undefined}
                        >
                          {healthMeta.label}
                          {healthMeta.detail ? ` · ${healthMeta.detail}` : ""}
                        </Badge>
                      )}
                      {discoveryStatusMeta && (
                        <Badge
                          tone={discoveryStatusMeta.tone}
                          title={discoveryStatusMeta.detail ?? undefined}
                        >
                          {discoveryStatusMeta.label}
                          {eventsData?.session
                            ? ` (${eventsData.session.format.replace(/_(rollout_)?jsonl$/, "")})`
                            : discoveryStatusMeta.detail
                              ? ` (${discoveryStatusMeta.detail})`
                              : ""}
                        </Badge>
                      )}
                      {selectedRssKib !== null && (
                        <span className="inline-flex items-center gap-1 font-mono text-micro text-muted-foreground">
                          <Cpu size={11} className="text-faint-foreground" />
                          VmRSS {formatRssKib(selectedRssKib)}
                          {selectedChildCount !== null && (
                            <span>
                              · {tr(locale, "子进程", "children")} {selectedChildCount}
                              {selectedProcessCount !== null ? `/${selectedProcessCount}` : ""}
                            </span>
                          )}
                        </span>
                      )}
                      {eventsData && (
                        <span className="ml-auto tnum font-mono text-micro text-muted-foreground">
                          {eventsData.summary.usage_totals.last_model
                            ? `${eventsData.summary.usage_totals.last_model} · `
                            : ""}
                          T{eventsData.summary.turn_count} · tools {eventsData.summary.tool_call_count}
                          {eventsData.summary.tool_error_count > 0 && (
                            <span className="text-destructive">
                              {" "}
                              ({eventsData.summary.tool_error_count} err)
                            </span>
                          )}
                          {" · "}
                          {formatTokenCount(eventsData.summary.usage_totals.input_tokens)}/
                          {formatTokenCount(eventsData.summary.usage_totals.output_tokens)} tok
                          {eventsData.summary.usage_totals.cache_read_tokens > 0
                            ? ` (${formatTokenCount(eventsData.summary.usage_totals.cache_read_tokens)} cached)`
                            : ""}
                        </span>
                      )}
                    </div>

                    {(selectedLeafCommand || (eventsData && eventsData.summary.in_flight_tools.length > 0)) && (
                      <div className="flex flex-wrap items-center gap-1.5">
                        {eventsData?.summary.in_flight_tools.map((tool, idx) => (
                          <Badge
                            key={tool.tool_call_id ?? `${tool.tool_name}-${idx}`}
                            tone="info"
                            dot
                            live
                            icon={<Wrench size={10} />}
                            title={tool.summary}
                          >
                            {tool.tool_name}
                            {tool.summary ? `: ${tool.summary.slice(0, 48)}` : ""}
                          </Badge>
                        ))}
                        {selectedLeafCommand && (
                          <span className="max-w-full truncate rounded bg-surface px-2 py-0.5 font-mono text-micro text-muted-foreground shadow-hairline">
                            {tr(locale, "叶进程", "leaf")}: {selectedLeafCommand}
                          </span>
                        )}
                      </div>
                    )}

                    {eventsData?.summary.last_error && (
                      <div className="truncate font-mono text-micro text-destructive">
                        {tr(locale, "最近错误", "Last error")}: {eventsData.summary.last_error}
                      </div>
                    )}
                    {eventsState.kind === "error" && (
                      <div className="truncate font-mono text-micro text-warning">
                        {tr(locale, "事件流轮询提示", "Events stream notice")}: {eventsState.message}
                      </div>
                    )}
                  </div>
                )}
              </Card>

              {/* Terminal Screen Viewport (`data-theme="dark"` required alongside `bg-terminal` per contrast.mjs) */}
              <section
                data-theme="dark"
                aria-label="Live terminal screen snapshot"
                className="flex min-h-[200px] flex-1 flex-col overflow-hidden rounded-card border border-surface-border bg-terminal shadow-card"
              >
                <header className="flex min-h-9 shrink-0 flex-wrap items-center gap-2 border-b border-white/[0.08] px-3.5 py-1.5 text-caption">
                  <span className="font-mono font-medium text-foreground">
                    Screen Capture
                  </span>
                  {activeSnapshot && (
                    <span className="font-mono text-micro text-muted-foreground">
                      {activeSnapshot.fidelity.fidelity === "styled_vt"
                        ? `${activeSnapshot.fidelity.styled_lines.length} styled lines`
                        : `${activeSnapshot.plain_lines.length} plain lines`}
                    </span>
                  )}
                  {scopedSnapshotState.kind === "ready" && (
                    <span className="ml-auto inline-flex items-center gap-1 font-mono text-micro text-faint-foreground">
                      <Clock size={11} />
                      {relativeTime(scopedSnapshotState.capturedAtMs)}
                    </span>
                  )}
                </header>

                <div className="min-h-0 flex-1 overflow-auto px-3.5 py-3 font-mono text-[12px] leading-5 text-terminal-foreground">
                  {activeSnapshot === null ? (
                    <div className="grid h-36 place-items-center text-caption text-faint-foreground">
                      正在获取屏幕快照…
                    </div>
                  ) : parsedStyledLines !== null ? (
                    <pre className="whitespace-pre-wrap break-words font-[inherit]">
                      {parsedStyledLines.length === 0 ? (
                        <span className="text-faint-foreground">(empty screen)</span>
                      ) : (
                        parsedStyledLines.map((spans, lineIdx) => (
                          <div key={lineIdx} className="min-h-5">
                            {spans.length === 0
                              ? " "
                              : spans.map((span, spanIdx) => (
                                  <span
                                    key={spanIdx}
                                    className={spanClassName(span)}
                                  >
                                    {span.text}
                                  </span>
                                ))}
                          </div>
                        ))
                      )}
                    </pre>
                  ) : (
                    <pre className="whitespace-pre-wrap break-words font-[inherit]">
                      {activeSnapshot.plain_lines.length === 0
                        ? " "
                        : activeSnapshot.plain_lines.map((line, lineIdx) => (
                            <div key={lineIdx} className="min-h-5">
                              {line || " "}
                            </div>
                          ))}
                    </pre>
                  )}
                </div>

                {activeSnapshot && activeSnapshot.choices.length > 0 && (
                  <div className="shrink-0 border-t border-white/[0.08] bg-white/[0.03] px-3.5 py-2">
                    <div className="mb-1 text-micro font-medium text-muted-foreground">
                      检测到的交互选项（choices）
                    </div>
                    <div className="flex flex-wrap gap-1.5">
                      {activeSnapshot.choices.map((choice) => (
                        <span
                          key={`${choice.line_index}:${choice.label}`}
                          className={cx(
                            "inline-flex items-center gap-1.5 rounded border px-2 py-0.5 font-mono text-micro",
                            choice.selected
                              ? "border-info bg-info-tint font-semibold text-info"
                              : "border-white/15 text-muted-foreground",
                          )}
                        >
                          {choice.selected && <span>❯</span>}
                          {choice.index !== null && <span>[{choice.index}]</span>}
                          <span>{choice.label}</span>
                        </span>
                      ))}
                    </div>
                  </div>
                )}
              </section>

              {/* Fail-Closed Banners: PlainTextOnly / Conflict / 403 / Result */}
              {!isReadOnly &&
                activeSnapshot &&
                activeSnapshot.fidelity.fidelity === "plain_text_only" && (
                  <div
                    role="status"
                    className="flex items-start gap-2.5 rounded-card border border-warning/30 bg-wait-tint px-3.5 py-2.5 text-caption text-warning"
                  >
                    <AlertTriangle size={15} className="mt-0.5 shrink-0" />
                    <div className="space-y-0.5">
                      <div className="font-semibold">
                        PlainTextOnly 降级模式：已禁用依赖选区校验的 guarded 动作
                      </div>
                      <p>
                        {capabilities.guarded.allowed ? "" : capabilities.guarded.message}
                      </p>
                    </div>
                  </div>
                )}

              {visibleActionState.kind === "conflict_guard_mismatch" && (
                <div
                  role="alert"
                  className="flex items-start gap-2.5 rounded-card border border-warning/40 bg-wait-tint px-3.5 py-3 text-caption text-warning"
                >
                  <AlertTriangle size={16} className="mt-0.5 shrink-0" />
                  <div className="min-w-0 flex-1 space-y-1">
                    <div className="font-semibold">
                      ScreenGuard 冲突（HTTP 409 guard_mismatch）— 已停止自动提交
                    </div>
                    <p className="break-words">{visibleActionState.message}</p>
                    <div className="flex flex-wrap gap-3 font-mono text-micro">
                      <span>期望旧 Guard: {visibleActionState.staleGuard ?? "—"}</span>
                      <span>
                        {visibleActionState.refreshPhase === "refreshing"
                          ? "正在自动刷新最新快照…"
                          : visibleActionState.refreshPhase === "refreshed"
                            ? `已自动刷新最新 Guard: ${visibleActionState.freshGuard ?? "—"}（请确认屏幕后手动重发）`
                            : "快照自动刷新失败，请点击右上角「刷新」后重发"}
                      </span>
                    </div>
                  </div>
                  <button
                    type="button"
                    onClick={() => setActionState({ kind: "idle" })}
                    className="shrink-0 rounded px-2 py-1 text-micro font-medium underline hover:opacity-80"
                  >
                    已知悉
                  </button>
                </div>
              )}

              {visibleActionState.kind === "blocked_unobservable_selection" && (
                <div
                  role="alert"
                  className="rounded-card bg-wait-tint px-3.5 py-2.5 text-caption text-warning"
                >
                  {visibleActionState.message}
                </div>
              )}

              {visibleActionState.kind === "forbidden_readonly" && (
                <div
                  role="alert"
                  className="flex items-start gap-2 rounded-card bg-fail-tint px-3.5 py-2.5 text-caption text-destructive"
                >
                  <ShieldAlert size={15} className="mt-0.5 shrink-0" />
                  <div>{visibleActionState.message}</div>
                </div>
              )}

              {visibleActionState.kind === "failed" && (
                <div
                  role="alert"
                  className="rounded-card bg-fail-tint px-3.5 py-2.5 text-caption text-destructive"
                >
                  {visibleActionState.message}
                </div>
              )}

              {visibleActionState.kind === "succeeded" && (
                <div
                  role="status"
                  className="flex items-center gap-2 rounded-card bg-ok-tint px-3.5 py-2.5 text-caption text-success"
                >
                  <CheckCircle2 size={15} className="shrink-0" />
                  <span>{formatActionOutcome(visibleActionState.outcome)}</span>
                </div>
              )}

              {/* RBAC-Gated Control Panel */}
              {isReadOnly ? (
                <Card className="p-3.5">
                  <div className="flex items-center gap-2 text-caption text-muted-foreground">
                    <Lock size={14} className="shrink-0 text-info" />
                    <span>
                      {capabilities.readOnlyReason ??
                        "只读权限受限：变更操作控件（SendText / SendKeys / Interrupt / WaitSettled）已隐藏并禁用。"}
                    </span>
                  </div>
                </Card>
              ) : (
                <Card className="space-y-3 p-3.5">
                  {/* SendText with SendPolicy selector */}
                  <div className="space-y-2">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <span className="text-caption font-medium text-foreground">
                        发送文本与策略（TuiAction::SendText）
                      </span>
                      <span className="text-micro text-faint-foreground">
                        {describePolicy(policyMode)}
                      </span>
                    </div>

                    <div className="flex flex-wrap gap-1.5" role="radiogroup" aria-label="发送策略">
                      {SEND_POLICY_OPTIONS.map((opt) => {
                        const needsStyled = isGuardedPolicyMode(opt.mode);
                        const optionDisabled =
                          needsStyled && !capabilities.guarded.allowed;
                        const active = policyMode === opt.mode;
                        return (
                          <button
                            key={opt.mode}
                            type="button"
                            role="radio"
                            aria-checked={active}
                            disabled={optionDisabled}
                            onClick={() => setPolicyMode(opt.mode)}
                            title={
                              optionDisabled && !capabilities.guarded.allowed
                                ? capabilities.guarded.message
                                : opt.hint
                            }
                            className={cx(
                              "inline-flex h-8 items-center rounded-control px-2.5 text-caption transition-colors pointer-coarse:h-11",
                              FOCUS_RING,
                              optionDisabled
                                ? "cursor-not-allowed bg-inset text-faint-foreground opacity-55"
                                : active
                                  ? "bg-surface-selected font-semibold text-foreground shadow-hairline"
                                  : "bg-inset text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                            )}
                          >
                            {opt.label}
                          </button>
                        );
                      })}
                    </div>

                    {isGuardedPolicyMode(policyMode) &&
                      !capabilities.guarded.allowed && (
                        <p className="text-micro text-warning">
                          当前所选 guarded 策略在 PlainTextOnly 或非存活终端下禁止发送，请切换非 guarded 策略（如「非阻塞时发送」）。
                        </p>
                      )}

                    <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
                      <textarea
                        rows={2}
                        value={textDraft}
                        onChange={(e) => setTextDraft(e.target.value)}
                        onKeyDown={(e) => {
                          if (
                            (e.metaKey || e.ctrlKey) &&
                            e.key === "Enter" &&
                            !actionSubmitting
                          ) {
                            e.preventDefault();
                            void handleSendText();
                          }
                        }}
                        disabled={!capabilities.canMutate || capabilities.terminalExited}
                        placeholder="输入要注入终端并回车提交的文本（Ctrl/Cmd + Enter 发送）…"
                        aria-label="发送给终端的文本"
                        className={cx(
                          "min-h-11 min-w-0 flex-1 resize-y rounded-control border border-surface-border bg-surface px-3 py-2 text-label text-foreground placeholder:text-faint-foreground",
                          FOCUS_RING,
                        )}
                      />
                      <div className="flex shrink-0 items-center gap-2">
                        <input
                          type="number"
                          min={1}
                          max={120000}
                          value={waitAfterSendMs}
                          onChange={(e) => setWaitAfterSendMs(e.target.value)}
                          placeholder="等待 ms (可选)"
                          aria-label="发送后等待稳定超时毫秒数（可选）"
                          className={cx(
                            "h-10 w-32 rounded-control border border-surface-border bg-surface px-2.5 font-mono text-caption text-foreground placeholder:text-faint-foreground pointer-coarse:h-11",
                            FOCUS_RING,
                          )}
                        />
                        <button
                          type="button"
                          disabled={
                            actionSubmitting ||
                            textDraft.length === 0 ||
                            capabilities.terminalExited ||
                            !activeSnapshot ||
                            (isGuardedPolicyMode(policyMode) &&
                              !capabilities.guarded.allowed)
                          }
                          onClick={() => void handleSendText()}
                          className={cx(
                            "inline-flex h-10 shrink-0 items-center gap-1.5 rounded-control bg-primary px-3.5 text-caption font-semibold text-primary-foreground shadow-btn transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                            FOCUS_RING,
                          )}
                        >
                          <Send size={13} />
                          发送文本
                        </button>
                      </div>
                    </div>
                  </div>

                  {/* Guarded SendKeys + Interrupt + WaitSettled */}
                  <div className="space-y-2 border-t border-surface-border pt-3">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <span className="text-caption font-medium text-foreground">
                        守卫按键与流程控制（SendKeys / Interrupt / WaitSettled）
                      </span>
                      {capabilities.guarded.allowed ? (
                        <span className="font-mono text-micro text-success">
                          {isValidScreenGuard(capabilities.guarded.guard)
                            ? `expected_guard=${capabilities.guarded.guard}`
                            : "guarded"}
                        </span>
                      ) : (
                        <span className="text-micro text-warning">
                          {capabilities.guarded.reason === "plain_text_only"
                            ? "PlainTextOnly 已禁止 guarded SendKeys"
                            : capabilities.guarded.message}
                        </span>
                      )}
                    </div>

                    <div className="flex flex-wrap items-center gap-1.5">
                      {QUICK_CHORDS.map((chord) => (
                        <button
                          key={chord}
                          type="button"
                          disabled={actionSubmitting || !capabilities.guarded.allowed}
                          onClick={() => void handleSendQuickChords([chord])}
                          className={cx(
                            "inline-flex h-8 min-w-9 items-center justify-center rounded-control border border-surface-border bg-inset px-2 font-mono text-caption font-medium text-foreground transition-colors hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                            FOCUS_RING,
                          )}
                        >
                          {chord}
                        </button>
                      ))}
                    </div>

                    <div className="flex flex-wrap items-center gap-2">
                      <input
                        type="text"
                        value={chordInput}
                        onChange={(e) => setChordInput(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter" && !actionSubmitting) {
                            e.preventDefault();
                            void handleSendCustomChords();
                          }
                        }}
                        disabled={!capabilities.guarded.allowed || actionSubmitting}
                        placeholder="按键序列（空格分隔，如 down enter 或 ctrl+c）"
                        aria-label="自定义 guarded 按键序列"
                        className={cx(
                          "h-9 min-w-48 flex-1 rounded-control border border-surface-border bg-surface px-2.5 font-mono text-caption text-foreground placeholder:text-faint-foreground disabled:opacity-50 pointer-coarse:h-11",
                          FOCUS_RING,
                        )}
                      />
                      <button
                        type="button"
                        disabled={
                          actionSubmitting ||
                          !capabilities.guarded.allowed ||
                          chordInput.trim().length === 0
                        }
                        onClick={() => void handleSendCustomChords()}
                        className={cx(
                          "inline-flex h-9 shrink-0 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                          FOCUS_RING,
                        )}
                      >
                        <CornerDownLeft size={13} />
                        发送按键
                      </button>

                      <button
                        type="button"
                        disabled={
                          actionSubmitting ||
                          !capabilities.canMutate ||
                          capabilities.terminalExited
                        }
                        onClick={() => void handleWaitSettled()}
                        className={cx(
                          "inline-flex h-9 shrink-0 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                          FOCUS_RING,
                        )}
                        title="发送 WaitSettled (timeout=5000ms)"
                      >
                        <Clock size={13} />
                        等待稳定
                      </button>

                      <button
                        type="button"
                        disabled={
                          actionSubmitting ||
                          !capabilities.canMutate ||
                          capabilities.terminalExited
                        }
                        onClick={() => void handleInterrupt()}
                        onBlur={() => setInterruptConfirming(false)}
                        onKeyDown={(e) => {
                          if (e.key === "Escape") setInterruptConfirming(false);
                        }}
                        className={cx(
                          "inline-flex h-9 shrink-0 items-center gap-1.5 rounded-control px-3 text-caption font-semibold shadow-hairline transition-colors disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                          FOCUS_RING,
                          interruptConfirming
                            ? "bg-fail-tint text-destructive"
                            : "bg-inset text-destructive hover:bg-fail-tint",
                        )}
                        title="发送 Interrupt (timeout=5000ms)；二次点击确认"
                      >
                        <OctagonX size={13} />
                        {interruptConfirming ? "确认中断" : "中断"}
                      </button>
                    </div>
                  </div>
                </Card>
              )}

              {/* AR-4A Observability Stream & /proc Subtree Panel */}
              <Card>
                <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-2.5">
                  <div className="flex flex-wrap items-center gap-2">
                    <Activity size={14} className="text-muted-foreground" />
                    <span className="text-caption font-semibold text-foreground">
                      {tr(locale, "实时可观测流与进程树", "Live Observability & Process Tree")}
                    </span>
                    {eventsData && (
                      <>
                        <Badge
                          tone={
                            eventsData.fidelity === "session_enriched"
                              ? "success"
                              : eventsData.fidelity === "control_only"
                                ? "info"
                                : "neutral"
                          }
                        >
                          {eventsData.fidelity}
                        </Badge>
                        {eventsData.cursorResetCount > 0 && (
                          <Badge
                            tone="warning"
                            title={
                              eventsData.lastCursorResetAtMs
                                ? `Last cursor_reset ${relativeTime(eventsData.lastCursorResetAtMs)}`
                                : undefined
                            }
                          >
                            {tr(
                              locale,
                              `游标已重置 ×${eventsData.cursorResetCount}`,
                              `cursor_reset ×${eventsData.cursorResetCount}`,
                            )}
                          </Badge>
                        )}
                      </>
                    )}
                  </div>

                  <div className="flex flex-wrap items-center gap-1.5">
                    <button
                      type="button"
                      onClick={() => {
                        setObservabilityTab("events");
                        setObservabilityExpanded(true);
                      }}
                      className={cx(chip(observabilityTab === "events"), FOCUS_RING)}
                    >
                      {tr(locale, "活动流", "Events")} ({eventsData?.events.length ?? 0})
                    </button>
                    <button
                      type="button"
                      onClick={() => {
                        setObservabilityTab("procs");
                        setObservabilityExpanded(true);
                      }}
                      className={cx(chip(observabilityTab === "procs"), FOCUS_RING)}
                    >
                      {tr(locale, "进程树", "Process Tree")} (
                      {eventsData?.process_tree?.processes.length ?? 0})
                    </button>
                    <button
                      type="button"
                      onClick={() => setObservabilityExpanded((v) => !v)}
                      aria-expanded={observabilityExpanded}
                      className={cx(
                        "inline-flex h-7 items-center gap-1 rounded-control bg-inset px-2 text-micro text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                        FOCUS_RING,
                      )}
                    >
                      {observabilityExpanded ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
                      {observabilityExpanded
                        ? tr(locale, "收起", "Collapse")
                        : tr(locale, "展开", "Expand")}
                    </button>
                  </div>
                </div>

                {observabilityExpanded && (
                  <div className="border-t border-surface-border px-4 py-3 text-caption">
                    {!eventsData ? (
                      <div className="py-4 text-center text-faint-foreground">
                        {eventsState.kind === "loading"
                          ? tr(locale, "正在获取增量可观测事件流…", "Loading incremental observability stream…")
                          : eventsState.kind === "error"
                            ? eventsState.message
                            : tr(locale, "暂无可观测数据", "No observability data")}
                      </div>
                    ) : observabilityTab === "events" ? (
                      <div className="space-y-2">
                        <div className="flex flex-wrap items-center justify-between gap-2 font-mono text-micro text-faint-foreground">
                          <div className="flex flex-wrap items-center gap-1.5">
                            <span>{tr(locale, "观测源", "Sources")}:</span>
                            {eventsData.summary.sources_observed.map((src) => (
                              <Badge key={src} tone="neutral">
                                {src}
                              </Badge>
                            ))}
                          </div>
                          <span>
                            latest_key: {eventsData.latest_key ?? tr(locale, "无（仅屏幕）", "none (screen_only)")}
                            {eventsData.truncated ? " · truncated" : ""}
                          </span>
                        </div>

                        {eventsData.events.length === 0 ? (
                          <div className="rounded-control bg-inset px-3 py-4 text-center text-muted-foreground">
                            {tr(locale, "当前窗口暂无事件记录", "No events in current window")}
                          </div>
                        ) : (
                          <ul className="max-h-72 space-y-1.5 overflow-y-auto pr-1">
                            {[...eventsData.events].reverse().map((ev) => {
                              const view = describeFusedEvent(ev, locale);
                              return (
                                <li
                                  key={ev.canonical_key}
                                  className="flex flex-wrap items-start gap-2 rounded-control bg-inset px-2.5 py-1.5"
                                >
                                  <Badge tone={view.tone}>{view.badge}</Badge>
                                  <div className="min-w-0 flex-1 space-y-0.5">
                                    <div className="break-words text-foreground">{view.text}</div>
                                    {view.detail && (
                                      <div className="truncate font-mono text-micro text-faint-foreground">
                                        {view.detail}
                                      </div>
                                    )}
                                  </div>
                                  <div className="flex shrink-0 items-center gap-1 font-mono text-micro text-faint-foreground">
                                    {ev.origins.map((origin) => (
                                      <span key={origin} className="rounded bg-surface px-1 py-px">
                                        {origin === "native_session"
                                          ? "session"
                                          : origin === "herdr_pty"
                                            ? "pty"
                                            : "ctrl"}
                                      </span>
                                    ))}
                                    {ev.timestamp_ms !== null && (
                                      <span>{relativeTime(ev.timestamp_ms)}</span>
                                    )}
                                  </div>
                                </li>
                              );
                            })}
                          </ul>
                        )}
                      </div>
                    ) : !eventsData.process_tree ? (
                      <div className="rounded-control bg-inset px-3 py-4 text-center text-muted-foreground">
                        {tr(
                          locale,
                          "暂无本地 /proc 进程树遥测（远端联邦终端或进程已退出）。",
                          "No local /proc process tree available (remote federated terminal or exited process).",
                        )}
                      </div>
                    ) : (
                      <div className="space-y-2">
                        <div className="flex flex-wrap items-center gap-3 font-mono text-micro text-muted-foreground">
                          <span>leader PID {eventsData.process_tree.leader_pid}</span>
                          <span>
                            {tr(locale, "进程数", "processes")} {eventsData.process_tree.process_count} (
                            {tr(locale, "子进程", "children")} {eventsData.process_tree.child_count})
                          </span>
                          <span>VmRSS {formatRssKib(eventsData.process_tree.total_rss_kib)}</span>
                          {eventsData.process_tree.active_leaf_command && (
                            <span className="truncate">
                              leaf: {eventsData.process_tree.active_leaf_command}
                            </span>
                          )}
                        </div>
                        <div className="max-h-72 overflow-auto rounded-control border border-surface-border">
                          <table className="w-full border-collapse text-left font-mono text-micro">
                            <thead className="sticky top-0 bg-surface text-faint-foreground">
                              <tr className="border-b border-surface-border">
                                <th className="px-2.5 py-1.5 font-medium">PID</th>
                                <th className="px-2.5 py-1.5 font-medium">PPID</th>
                                <th className="px-2.5 py-1.5 font-medium">State</th>
                                <th className="px-2.5 py-1.5 font-medium">VmRSS</th>
                                <th className="px-2.5 py-1.5 font-medium">comm</th>
                                <th className="px-2.5 py-1.5 font-medium">
                                  {tr(locale, "命令行（已脱敏）", "Command (redacted)")}
                                </th>
                              </tr>
                            </thead>
                            <tbody className="divide-y divide-surface-border/60 bg-inset/40">
                              {eventsData.process_tree.processes.map((proc) => (
                                <tr key={proc.pid}>
                                  <td className="tnum px-2.5 py-1 text-foreground">{proc.pid}</td>
                                  <td className="tnum px-2.5 py-1 text-muted-foreground">{proc.ppid}</td>
                                  <td className="px-2.5 py-1 text-muted-foreground">{proc.state}</td>
                                  <td className="tnum px-2.5 py-1 text-muted-foreground">
                                    {formatRssKib(proc.rss_kib)}
                                  </td>
                                  <td className="px-2.5 py-1 text-foreground">{proc.comm}</td>
                                  <td className="max-w-sm truncate px-2.5 py-1 text-faint-foreground" title={proc.command}>
                                    {proc.command}
                                  </td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        </div>
                      </div>
                    )}
                  </div>
                )}
              </Card>
            </div>
          )}
        </section>
      </div>
    </div>
  );
}
