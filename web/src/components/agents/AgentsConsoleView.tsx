import {
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  CornerDownLeft,
  Clock,
  Lock,
  OctagonX,
  RefreshCw,
  Search,
  Send,
  ShieldAlert,
  SquareTerminal,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  agentSnapshotUrl,
  buildSendKeysAction,
  buildSendTextAction,
  defaultSendPolicyMode,
  effectiveIncludeAll,
  evaluateActionCapabilities,
  isGuardedPolicyMode,
  isValidScreenGuard,
  parseKeyChordList,
  parseStyledLine,
  reduceAction,
  reduceDiscovery,
  reduceSnapshot,
  snapshotFromState,
  visibleAgentsForRole,
  type ActionSubmissionState,
  type AgentDiscoveryState,
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
  fetchGlobalAgentSnapshot,
  fetchGlobalAgents,
  postGlobalAgentAction,
} from "../../agentRuntimeApi";
import { relativeTime } from "../../format";
import { pageHidden } from "../../validated";
import type { SessionRole } from "../../token.ts";
import { Button, Card, CardHeader, EmptyState, FOCUS_RING, FOCUS_RING_INSET, PageHeader, chip, cx } from "../ui";
import { Badge, type BadgeTone } from "../ui/badge";

const DISCOVERY_POLL_MS = 3_000;
const SNAPSHOT_POLL_MS = 2_000;
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
  const [requestedAll, setRequestedAll] = useState(false);
  const [originFilter, setOriginFilter] = useState<OriginFilter>("all");
  const [query, setQuery] = useState("");
  const [refreshSeq, setRefreshSeq] = useState(0);
  const [mobileDetailOpen, setMobileDetailOpen] = useState(() => selectedAddress !== null);
  const [serverReadOnlyRefused, setServerReadOnlyRefused] = useState(false);

  const includeAll = effectiveIncludeAll(role, requestedAll);
  const isReadOnly = role.kind === "viewer" || serverReadOnlyRefused;

  const [discovery, setDiscovery] = useState<AgentDiscoveryState>(() => ({
    kind: "loading",
    includeAll,
  }));
  const [snapshotState, setSnapshotState] = useState<SnapshotLoadState>(() =>
    selectedAddress
      ? { kind: "loading", address: selectedAddress, previous: null }
      : { kind: "idle" },
  );
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
      policyInitAddressRef.current = null;
    } else {
      setSnapshotState((curr) => reduceSnapshot(curr, { type: "selected", address: selectedAddress }));
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

  const activeSnapshot: TuiSnapshot | null = snapshotFromState(snapshotState);
  const selectedSummary = useMemo(
    () => rawAgents.find((a) => a.address === selectedAddress) ?? null,
    [rawAgents, selectedAddress],
  );
  const capabilities = useMemo(
    () => evaluateActionCapabilities(role, serverReadOnlyRefused, snapshotState),
    [role, serverReadOnlyRefused, snapshotState],
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
    [refreshSnapshotOnce],
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
                只读观察 · ReadOnlyViewer
                {role.kind === "viewer" ? ` (${relativeTime(role.expiresAtMs)}到期)` : ""}
              </Badge>
            ) : (
              <Badge tone="success" size="md">
                操作模式
              </Badge>
            )}
          </>
        }
      >
        {!isReadOnly && (
          <button
            type="button"
            onClick={() => setRequestedAll((v) => !v)}
            aria-pressed={includeAll}
            className={cx(
              chip(includeAll),
              "pointer-coarse:h-10",
              FOCUS_RING,
            )}
            title="切换 GET /api/agents?all=true（包含已退出或外部终端）"
          >
            {includeAll ? "包含已退出 (?all=true)" : "仅运行中"}
          </button>
        )}
        <Button
          onClick={() => setRefreshSeq((s) => s + 1)}
          className="pointer-coarse:h-10"
          title="立即刷新全局终端列表与当前屏幕快照"
        >
          <RefreshCw size={13} />
          刷新
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
                        ? "当前无运行中的 Spawn 受管 Herdr 终端（ReadOnlyViewer 不会显示外部非受管终端）。"
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
          ) : snapshotState.kind === "forbidden_external" ? (
            <Card className="p-5">
              <div className="flex items-start gap-3">
                <ShieldAlert size={18} className="mt-0.5 shrink-0 text-destructive" />
                <div className="space-y-1">
                  <h2 className="text-body font-semibold text-foreground">
                    外部终端观察已拦截（HTTP 403）
                  </h2>
                  <p className="text-caption text-destructive">
                    {snapshotState.message}
                  </p>
                  <p className="text-caption text-muted-foreground">
                    `ReadOnlyViewer` 凭据仅允许列出和读取 Spawn 受管终端的屏幕快照；未触发任何外部读屏操作。
                  </p>
                </div>
              </div>
            </Card>
          ) : snapshotState.kind === "unauthorized" ? (
            <Card className="p-5">
              <p className="text-caption text-destructive">
                {snapshotState.message}
              </p>
            </Card>
          ) : snapshotState.kind === "not_found" ? (
            <Card className="p-5">
              <EmptyState
                title={`终端 ${snapshotState.address} 已不存在或已释放`}
                hint={
                  <div className="flex flex-col items-center gap-3">
                    <span>{snapshotState.message}</span>
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
                          title={`Endpoint: ${agentSnapshotUrl(selectedAddress)} | snapshot_hash=${activeSnapshot.snapshot_hash}`}
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

                {snapshotState.kind === "error" && (
                  <div className="border-t border-surface-border bg-fail-tint px-4 py-2 text-caption text-destructive">
                    {snapshotState.message}
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
                  {snapshotState.kind === "ready" && (
                    <span className="ml-auto inline-flex items-center gap-1 font-mono text-micro text-faint-foreground">
                      <Clock size={11} />
                      {relativeTime(snapshotState.capturedAtMs)}
                    </span>
                  )}
                </header>

                <div className="min-h-0 flex-1 overflow-auto px-3.5 py-3 font-mono text-[12px] leading-5 text-terminal-foreground">
                  {activeSnapshot === null ? (
                    <div className="grid h-36 place-items-center text-caption text-faint-foreground">
                      正在获取屏幕快照…
                    </div>
                  ) : activeSnapshot.fidelity.fidelity === "styled_vt" ? (
                    <pre className="whitespace-pre-wrap break-words font-[inherit]">
                      {activeSnapshot.fidelity.styled_lines.length === 0 ? (
                        <span className="text-faint-foreground">(empty screen)</span>
                      ) : (
                        activeSnapshot.fidelity.styled_lines.map((rawLine, lineIdx) => {
                          const spans = parseStyledLine(rawLine);
                          return (
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
                          );
                        })
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

              {actionState.kind === "conflict_guard_mismatch" && (
                <div
                  role="alert"
                  className="flex items-start gap-2.5 rounded-card border border-warning/40 bg-wait-tint px-3.5 py-3 text-caption text-warning"
                >
                  <AlertTriangle size={16} className="mt-0.5 shrink-0" />
                  <div className="min-w-0 flex-1 space-y-1">
                    <div className="font-semibold">
                      ScreenGuard 冲突（HTTP 409 guard_mismatch）— 已停止自动提交
                    </div>
                    <p className="break-words">{actionState.message}</p>
                    <div className="flex flex-wrap gap-3 font-mono text-micro">
                      <span>期望旧 Guard: {actionState.staleGuard ?? "—"}</span>
                      <span>
                        {actionState.refreshPhase === "refreshing"
                          ? "正在自动刷新最新快照…"
                          : actionState.refreshPhase === "refreshed"
                            ? `已自动刷新最新 Guard: ${actionState.freshGuard ?? "—"}（请确认屏幕后手动重发）`
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

              {actionState.kind === "blocked_unobservable_selection" && (
                <div
                  role="alert"
                  className="rounded-card bg-wait-tint px-3.5 py-2.5 text-caption text-warning"
                >
                  {actionState.message}
                </div>
              )}

              {actionState.kind === "forbidden_readonly" && (
                <div
                  role="alert"
                  className="flex items-start gap-2 rounded-card bg-fail-tint px-3.5 py-2.5 text-caption text-destructive"
                >
                  <ShieldAlert size={15} className="mt-0.5 shrink-0" />
                  <div>{actionState.message}</div>
                </div>
              )}

              {actionState.kind === "failed" && (
                <div
                  role="alert"
                  className="rounded-card bg-fail-tint px-3.5 py-2.5 text-caption text-destructive"
                >
                  {actionState.message}
                </div>
              )}

              {actionState.kind === "succeeded" && (
                <div
                  role="status"
                  className="flex items-center gap-2 rounded-card bg-ok-tint px-3.5 py-2.5 text-caption text-success"
                >
                  <CheckCircle2 size={15} className="shrink-0" />
                  <span>{formatActionOutcome(actionState.outcome)}</span>
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
            </div>
          )}
        </section>
      </div>
    </div>
  );
}
