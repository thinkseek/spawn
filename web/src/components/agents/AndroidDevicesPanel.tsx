import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  Eye,
  Home,
  Lock,
  Play,
  RefreshCw,
  ShieldAlert,
  Smartphone,
  Undo2,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  fetchDevices,
  flattenUiNodes,
  postDeviceCommand,
  type DeviceActionName,
  type DeviceSnapshot,
  type FlattenedUiNode,
} from "../../devicesApi";
import { relativeTime } from "../../format";
import { pageHidden } from "../../validated";
import { Button, Card, CardHeader, EmptyState, FOCUS_RING, cx, tr, useLocale } from "../ui";
import { Badge, type BadgeTone } from "../ui/badge";

interface AndroidDevicesPanelProps {
  canWrite: boolean;
}

const POLL_INTERVAL_MS = 3_000;

function commandStatusBadge(status: string): { label: string; tone: BadgeTone } {
  switch (status) {
    case "executed":
      return { label: "executed", tone: "success" };
    case "pending":
      return { label: "pending", tone: "info" };
    case "dispatched":
      return { label: "dispatched", tone: "info" };
    case "needs_confirmation":
      return { label: "needs_confirmation", tone: "warning" };
    case "denied":
      return { label: "denied", tone: "danger" };
    default:
      return { label: status, tone: "danger" };
  }
}

export default function AndroidDevicesPanel({ canWrite }: AndroidDevicesPanelProps) {
  const { locale } = useLocale();
  const [devices, setDevices] = useState<DeviceSnapshot[]>([]);
  const [loading, setLoading] = useState<boolean>(true);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [refreshSeq, setRefreshSeq] = useState<number>(0);

  const [submitting, setSubmitting] = useState<boolean>(false);
  const [feedback, setFeedback] = useState<{ tone: "ok" | "err"; text: string } | null>(null);
  const [packageInput, setPackageInput] = useState<string>("com.android.settings");
  const [clickX, setClickX] = useState<string>("540");
  const [clickY, setClickY] = useState<string>("960");
  const [targetNodeId, setTargetNodeId] = useState<string>("");
  const [inputTextDraft, setInputTextDraft] = useState<string>("");

  useEffect(() => {
    let cancelled = false;
    let inFlight = false;
    let controller: AbortController | null = null;

    const tick = async () => {
      if (pageHidden(document) || inFlight) return;
      inFlight = true;
      controller = new AbortController();
      try {
        const res = await fetchDevices(controller.signal);
        if (cancelled) return;
        setLoading(false);
        if (res.ok) {
          setErrorMsg(null);
          setDevices(res.data.devices);
        } else {
          setErrorMsg(res.error.message);
        }
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
      } finally {
        if (!cancelled) inFlight = false;
      }
    };

    const onVis = () => {
      if (!pageHidden(document)) void tick();
    };
    void tick();
    const timer = window.setInterval(() => void tick(), POLL_INTERVAL_MS);
    document.addEventListener("visibilitychange", onVis);
    return () => {
      cancelled = true;
      controller?.abort();
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVis);
    };
  }, [refreshSeq]);

  useEffect(() => {
    if (devices.length === 0) {
      setSelectedId(null);
      return;
    }
    if (!selectedId || !devices.some((d) => d.device_id === selectedId)) {
      setSelectedId(devices[0].device_id);
    }
  }, [devices, selectedId]);

  const selected = useMemo(
    () => devices.find((d) => d.device_id === selectedId) ?? null,
    [devices, selectedId],
  );

  const uiNodes = useMemo<FlattenedUiNode[]>(
    () => (selected?.latest_observation ? flattenUiNodes(selected.latest_observation) : []),
    [selected],
  );

  const canActOnSelected = Boolean(
    canWrite &&
      selected &&
      selected.relay_enabled &&
      !selected.emergency_stopped,
  );

  const sendAction = useCallback(
    async (action: DeviceActionName, params: Record<string, unknown>) => {
      if (!selected || !canActOnSelected || submitting) return;
      setSubmitting(true);
      setFeedback(null);
      try {
        const res = await postDeviceCommand(selected.device_id, action, params);
        if (res.ok) {
          setFeedback({
            tone: "ok",
            text: `已下发 ${action} -> ${res.data.command_id} (${res.data.status})`,
          });
          setRefreshSeq((s) => s + 1);
        } else {
          setFeedback({ tone: "err", text: res.error.message });
        }
      } finally {
        setSubmitting(false);
      }
    },
    [selected, canActOnSelected, submitting],
  );

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3 sm:p-4">
      <Card>
        <CardHeader
          icon={<Smartphone size={14} />}
          title={tr(locale, "Android 客户端受控中继（android_gui v1）", "Android Device Control Relay")}
          meta={
            <div className="flex items-center gap-2">
              <Badge tone="neutral" count={devices.length} />
              <Button onClick={() => setRefreshSeq((s) => s + 1)} className="h-7 px-2.5 text-micro">
                <RefreshCw size={12} />
                {tr(locale, "刷新", "Refresh")}
              </Button>
            </div>
          }
        />

        {!canWrite && (
          <div className="flex items-center gap-2 border-b border-surface-border bg-info-tint px-4 py-2 text-caption text-info">
            <Lock size={14} />
            <span>
              {tr(
                locale,
                "当前为 ReadOnlyViewer 只读凭证：仅允许查看已连接 Android 设备与脱敏控件树，所有控制指令已禁用。",
                "ReadOnlyViewer credentials allow observing registered devices only; all control actions are disabled.",
              )}
            </span>
          </div>
        )}

        {errorMsg && (
          <div className="border-b border-surface-border bg-fail-tint px-4 py-2 text-caption text-destructive">
            {errorMsg}
          </div>
        )}

        <div className="p-3 sm:p-4">
          {loading && devices.length === 0 ? (
            <div className="py-8 text-center text-caption text-faint-foreground">
              {tr(locale, "正在加载已注册的 Android 设备…", "Loading enrolled Android devices…")}
            </div>
          ) : devices.length === 0 ? (
            <EmptyState
              icon={<Smartphone size={20} />}
              title={tr(locale, "暂无在线的 Android 控制端设备", "No Android device connected")}
              hint={tr(
                locale,
                "请在 Spawn Android 客户端进入「设备」页：1) 授权无障碍服务与常驻急停通知；2) 打开「允许远程设备操控」；3) 打开「开启 Spawn 服务器远程接管中继」，设备将自动注册到此面板。",
                "Open the Device tab in Spawn Android, grant Accessibility & Stop Notification, enable Remote Control, and turn on Spawn Server Relay.",
              )}
              className="py-8"
            />
          ) : (
            <div className="flex flex-wrap gap-2">
              {devices.map((dev) => {
                const active = dev.device_id === selected?.device_id;
                return (
                  <button
                    key={dev.device_id}
                    type="button"
                    onClick={() => setSelectedId(dev.device_id)}
                    className={cx(
                      "flex min-w-52 flex-col gap-1 rounded-control border px-3 py-2 text-left transition-colors pointer-coarse:min-h-11",
                      FOCUS_RING,
                      active
                        ? "border-primary bg-surface-selected"
                        : "border-surface-border bg-inset hover:bg-surface-hover",
                    )}
                  >
                    <div className="flex items-center gap-2">
                      <span className="font-mono text-label font-semibold text-foreground">
                        {dev.label}
                      </span>
                      <Badge tone={dev.online ? "success" : "neutral"} dot live={dev.online}>
                        {dev.online ? "在线" : "离线"}
                      </Badge>
                      {dev.emergency_stopped && <Badge tone="danger">急停中</Badge>}
                    </div>
                    <div className="flex flex-wrap items-center gap-1.5 font-mono text-micro text-muted-foreground">
                      <span>{dev.device_id}</span>
                      {dev.active_package && <span>· {dev.active_package}</span>}
                      <span>· {relativeTime(dev.last_seen_ms)}</span>
                    </div>
                  </button>
                );
              })}
            </div>
          )}
        </div>
      </Card>

      {selected && (
        <>
          <Card className="space-y-3 p-4">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap items-center gap-1.5">
                <Badge tone={selected.control_enabled ? "success" : "warning"}>
                  控制开关: {selected.control_enabled ? "开" : "关"}
                </Badge>
                <Badge tone={selected.accessibility_ready ? "success" : "warning"}>
                  无障碍: {selected.accessibility_ready ? "就绪" : "未授权"}
                </Badge>
                <Badge tone={selected.notifications_ready ? "success" : "warning"}>
                  急停通知: {selected.notifications_ready ? "就绪" : "未就绪"}
                </Badge>
                <Badge tone={selected.relay_enabled ? "info" : "neutral"}>
                  服务器中继: {selected.relay_enabled ? "已开启" : "已关闭"}
                </Badge>
                {selected.password_redacted_count > 0 && (
                  <Badge tone="success">
                    已脱敏 {selected.password_redacted_count} 处密码节点
                  </Badge>
                )}
              </div>
              <span className="font-mono text-micro text-faint-foreground">
                节点数 {selected.node_count}
              </span>
            </div>

            {selected.emergency_stopped && (
              <div className="flex items-center gap-2 rounded-control border border-destructive/40 bg-fail-tint px-3 py-2 text-caption text-destructive">
                <ShieldAlert size={15} />
                <span>设备已触发急停保护：所有待执行队列已自动阻断，须先在 Android 本机点击「恢复就绪」。</span>
              </div>
            )}

            {!selected.emergency_stopped && !selected.relay_enabled && (
              <div className="flex items-center gap-2 rounded-control border border-warning/40 bg-wait-tint px-3 py-2 text-caption text-warning">
                <AlertTriangle size={15} />
                <span>设备未开启 Spawn 服务器远程接管中继，请先在 Android 客户端开启中继开关。</span>
              </div>
            )}

            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                disabled={!canActOnSelected || submitting}
                onClick={() => void sendAction("screen_observe", {})}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-primary px-3 text-caption font-semibold text-primary-foreground shadow-btn transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                <Eye size={13} />
                刷新 UI 控件树
              </button>
              <button
                type="button"
                disabled={!canActOnSelected || submitting}
                onClick={() => void sendAction("global_action", { kind: "back" })}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                <Undo2 size={13} />
                返回
              </button>
              <button
                type="button"
                disabled={!canActOnSelected || submitting}
                onClick={() => void sendAction("global_action", { kind: "home" })}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                <Home size={13} />
                主页
              </button>
              <button
                type="button"
                disabled={!canActOnSelected || submitting}
                onClick={() => void sendAction("global_action", { kind: "recents" })}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                多任务
              </button>
              <button
                type="button"
                disabled={!canActOnSelected || submitting}
                onClick={() => void sendAction("scroll", { direction: "up" })}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                <ArrowUp size={13} />
                上滑
              </button>
              <button
                type="button"
                disabled={!canActOnSelected || submitting}
                onClick={() => void sendAction("scroll", { direction: "down" })}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                <ArrowDown size={13} />
                下滑
              </button>
            </div>

            <div className="grid gap-2 md:grid-cols-2">
              <div className="flex items-center gap-2">
                <input
                  type="text"
                  value={packageInput}
                  onChange={(e) => setPackageInput(e.target.value)}
                  disabled={!canActOnSelected}
                  placeholder="应用包名，如 com.android.settings"
                  aria-label="启动应用包名"
                  className={cx(
                    "h-9 min-w-0 flex-1 rounded-control border border-surface-border bg-surface px-2.5 font-mono text-caption text-foreground pointer-coarse:h-11",
                    FOCUS_RING,
                  )}
                />
                <button
                  type="button"
                  disabled={!canActOnSelected || submitting || !packageInput.trim()}
                  onClick={() =>
                    void sendAction("launch_app", { package_name: packageInput.trim() })
                  }
                  className={cx(
                    "inline-flex h-9 shrink-0 items-center gap-1.5 rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                    FOCUS_RING,
                  )}
                >
                  <Play size={12} />
                  启动应用
                </button>
              </div>

              <div className="flex items-center gap-2">
                <input
                  type="number"
                  value={clickX}
                  onChange={(e) => setClickX(e.target.value)}
                  disabled={!canActOnSelected}
                  placeholder="X"
                  aria-label="点击 X 坐标"
                  className={cx(
                    "h-9 w-20 rounded-control border border-surface-border bg-surface px-2 font-mono text-caption text-foreground pointer-coarse:h-11",
                    FOCUS_RING,
                  )}
                />
                <input
                  type="number"
                  value={clickY}
                  onChange={(e) => setClickY(e.target.value)}
                  disabled={!canActOnSelected}
                  placeholder="Y"
                  aria-label="点击 Y 坐标"
                  className={cx(
                    "h-9 w-20 rounded-control border border-surface-border bg-surface px-2 font-mono text-caption text-foreground pointer-coarse:h-11",
                    FOCUS_RING,
                  )}
                />
                <button
                  type="button"
                  disabled={!canActOnSelected || submitting}
                  onClick={() => {
                    const x = Number.parseInt(clickX, 10);
                    const y = Number.parseInt(clickY, 10);
                    if (Number.isInteger(x) && Number.isInteger(y)) {
                      void sendAction("click_xy", { x, y });
                    }
                  }}
                  className={cx(
                    "inline-flex h-9 shrink-0 items-center rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                    FOCUS_RING,
                  )}
                >
                  点击坐标
                </button>
              </div>
            </div>

            <div className="flex flex-wrap items-center gap-2">
              <input
                type="text"
                value={targetNodeId}
                onChange={(e) => setTargetNodeId(e.target.value)}
                disabled={!canActOnSelected}
                placeholder="目标 node_id（如 0.1.0）"
                aria-label="输入目标控件 node_id"
                className={cx(
                  "h-9 w-40 rounded-control border border-surface-border bg-surface px-2.5 font-mono text-caption text-foreground pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              />
              <input
                type="text"
                value={inputTextDraft}
                onChange={(e) => setInputTextDraft(e.target.value)}
                disabled={!canActOnSelected}
                placeholder="输入到控件的文本（密码输入框会被手机护栏硬拦截）"
                aria-label="输入控件文本"
                className={cx(
                  "h-9 min-w-48 flex-1 rounded-control border border-surface-border bg-surface px-2.5 text-caption text-foreground pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              />
              <button
                type="button"
                disabled={!canActOnSelected || submitting || !targetNodeId.trim()}
                onClick={() =>
                  void sendAction("input_text", {
                    node_id: targetNodeId.trim(),
                    text: inputTextDraft,
                  })
                }
                className={cx(
                  "inline-flex h-9 shrink-0 items-center rounded-control bg-inset px-3 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                写入文本
              </button>
            </div>

            {feedback && (
              <div
                className={cx(
                  "flex items-center gap-2 rounded-control px-3 py-2 text-caption",
                  feedback.tone === "ok"
                    ? "bg-ok-tint text-success"
                    : "bg-fail-tint text-destructive",
                )}
              >
                {feedback.tone === "ok" ? <CheckCircle2 size={14} /> : <AlertTriangle size={14} />}
                <span>{feedback.text}</span>
              </div>
            )}
          </Card>

          <div className="grid gap-3 lg:grid-cols-2">
            {/* Redacted UiNode Tree */}
            <Card className="flex min-h-72 flex-col">
              <CardHeader
                title={tr(locale, "实时脱敏 UI 控件树", "Redacted UI Tree")}
                meta={
                  selected.latest_observed_at_ms ? (
                    <span className="tnum font-mono text-micro">
                      {relativeTime(selected.latest_observed_at_ms)} · {uiNodes.length} nodes
                    </span>
                  ) : undefined
                }
              />
              <div className="max-h-96 flex-1 overflow-y-auto p-3">
                {uiNodes.length === 0 ? (
                  <p className="py-8 text-center text-caption text-faint-foreground">
                    暂无最新屏幕捕获。点击上方「刷新 UI 控件树」向 Android 客户端下发一次 `screen_observe`。
                  </p>
                ) : (
                  <ul className="space-y-1 font-mono text-micro">
                    {uiNodes.map((node) => (
                      <li
                        key={node.id}
                        style={{ paddingLeft: `${Math.min(node.depth, 10) * 12}px` }}
                        className="flex flex-wrap items-center gap-1.5 rounded px-1.5 py-1 hover:bg-surface-hover"
                      >
                        <span className="text-muted-foreground">[{node.id}]</span>
                        <span className="text-foreground">
                          {node.className.split(".").pop() || "View"}
                        </span>
                        {node.isPassword ? (
                          <Badge tone="warning">[REDACTED]</Badge>
                        ) : node.text ? (
                          <span className="max-w-52 truncate text-accent">
                            &quot;{node.text}&quot;
                          </span>
                        ) : node.description ? (
                          <span className="max-w-52 truncate text-muted-foreground">
                            ({node.description})
                          </span>
                        ) : null}
                        {node.resourceId && (
                          <span className="truncate text-faint-foreground">
                            #{node.resourceId.split("/").pop()}
                          </span>
                        )}
                        <div className="ml-auto flex items-center gap-1">
                          {node.editable && !node.isPassword && (
                            <button
                              type="button"
                              onClick={() => setTargetNodeId(node.id)}
                              className="rounded border border-surface-border bg-inset px-1.5 py-0.5 text-[11px] text-foreground hover:bg-surface-hover"
                            >
                              选为输入目标
                            </button>
                          )}
                          {node.clickable && (
                            <button
                              type="button"
                              disabled={!canActOnSelected || submitting}
                              onClick={() => void sendAction("click_node", { node_id: node.id })}
                              className="rounded bg-primary/15 px-1.5 py-0.5 text-[11px] font-medium text-primary hover:bg-primary/25 disabled:opacity-40"
                            >
                              点击
                            </button>
                          )}
                        </div>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            </Card>

            {/* Recent Commands */}
            <Card className="flex min-h-72 flex-col">
              <CardHeader
                title={tr(locale, "中继指令队列与手机端裁决", "Relay Queue & Verdicts")}
                meta={<Badge tone="neutral" count={selected.commands.length} />}
              />
              <div className="max-h-96 flex-1 overflow-y-auto p-3">
                {selected.commands.length === 0 ? (
                  <p className="py-8 text-center text-caption text-faint-foreground">
                    暂无远程中继指令记录。
                  </p>
                ) : (
                  <ul className="space-y-1.5">
                    {selected.commands.map((cmd) => {
                      const badge = commandStatusBadge(cmd.status);
                      return (
                        <li
                          key={cmd.command_id}
                          className="rounded-control border border-surface-border bg-inset px-3 py-2 text-caption"
                        >
                          <div className="flex items-center gap-2">
                            <span className="font-mono font-semibold text-foreground">
                              {cmd.action}
                            </span>
                            <Badge tone={badge.tone}>{badge.label}</Badge>
                            <span className="ml-auto font-mono text-micro text-faint-foreground">
                              {relativeTime(cmd.created_at_ms)}
                            </span>
                          </div>
                          {(cmd.message || cmd.reason) && (
                            <p className="mt-1 font-mono text-micro text-muted-foreground">
                              {cmd.message ?? cmd.reason}
                            </p>
                          )}
                        </li>
                      );
                    })}
                  </ul>
                )}
              </div>
            </Card>
          </div>
        </>
      )}
    </div>
  );
}
