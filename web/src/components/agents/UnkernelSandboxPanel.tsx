import {
  AlertTriangle,
  Cpu,
  Play,
  RotateCcw,
  Square,
  Terminal,
  Trash2,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  checkSupport,
  createKernel,
  MemoryFs,
  type InstanceStats,
  type KernelInstance,
  type SupportReport,
} from "unkernel/web";

import { Button, Card, CardHeader, EmptyState, FOCUS_RING, cx, tr, useLocale } from "../ui";
import { Badge } from "../ui/badge";

interface UnkernelSandboxPanelProps {
  principalLabel: string;
  canWrite: boolean;
  agentsCount: number;
}

type SandboxStatus = "checking" | "unsupported" | "idle" | "booting" | "running" | "exited" | "error";

const MAX_TERMINAL_CHARS = 32_000;

function trimOutput(next: string): string {
  if (next.length <= MAX_TERMINAL_CHARS) return next;
  return next.slice(next.length - MAX_TERMINAL_CHARS);
}

export default function UnkernelSandboxPanel({
  principalLabel,
  canWrite,
  agentsCount,
}: UnkernelSandboxPanelProps) {
  const { locale } = useLocale();
  const [support, setSupport] = useState<SupportReport | null>(null);
  const [status, setStatus] = useState<SandboxStatus>("checking");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [exitInfo, setExitInfo] = useState<string | null>(null);
  const [output, setOutput] = useState<string>("");
  const [commandInput, setCommandInput] = useState<string>("");
  const [stats, setStats] = useState<InstanceStats | null>(null);
  const [mountSpawnContext, setMountSpawnContext] = useState<boolean>(true);
  const [terminalFocused, setTerminalFocused] = useState<boolean>(false);

  const kernelRef = useRef<KernelInstance | null>(null);
  const unsubRef = useRef<(() => void) | null>(null);
  const outputViewportRef = useRef<HTMLDivElement | null>(null);
  const mountedRef = useRef<boolean>(true);
  const bootSeqRef = useRef<number>(0);

  const cleanupKernel = useCallback(async () => {
    bootSeqRef.current += 1;
    unsubRef.current?.();
    unsubRef.current = null;
    const active = kernelRef.current;
    kernelRef.current = null;
    if (active) {
      await active.stop({ code: 0, reason: "user_stopped" }).catch(() => undefined);
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    let cancelled = false;
    void checkSupport()
      .then((report) => {
        if (cancelled) return;
        setSupport(report);
        setStatus(report.ok ? "idle" : "unsupported");
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setStatus("error");
        setErrorMsg(cause instanceof Error ? cause.message : "Browser capability check failed");
      });
    return () => {
      cancelled = true;
      mountedRef.current = false;
      void cleanupKernel();
    };
  }, [cleanupKernel]);

  useEffect(() => {
    const el = outputViewportRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [output]);

  useEffect(() => {
    if (status !== "running") return;
    const timer = window.setInterval(() => {
      const k = kernelRef.current;
      if (!k) return;
      try {
        setStats(k.stats());
      } catch {
        // ignore transient stats read error on teardown
      }
    }, 1_000);
    return () => window.clearInterval(timer);
  }, [status]);

  const bootSandbox = useCallback(async () => {
    if (!support?.ok) return;
    setErrorMsg(null);
    setExitInfo(null);
    setStats(null);
    setStatus("booting");
    await cleanupKernel();
    const seq = bootSeqRef.current + 1;
    bootSeqRef.current = seq;

    setOutput(
      tr(
        locale,
        "[unkernel] 正在下载/初始化浏览器内置 Linux 内核 (single-CPU WebAssembly, 无需 COOP/COEP)…\n",
        "[unkernel] Booting in-browser Linux sandbox (single-CPU WebAssembly, no COOP/COEP needed)…\n",
      ),
    );

    try {
      const mounts = mountSpawnContext
        ? {
            "/spawn": {
              fs: new MemoryFs({
                "context.json": JSON.stringify(
                  {
                    source: "spawn-web-console",
                    principal: principalLabel,
                    can_write: canWrite,
                    herdr_agents_visible: agentsCount,
                    mounted_at: new Date().toISOString(),
                  },
                  null,
                  2,
                ),
                "README.txt":
                  "Mounted read-only from Spawn Web Console into /spawn inside unkernel Linux sandbox.\n",
              }),
              readOnly: true,
            },
          }
        : undefined;

      const kernel = await createKernel({
        worker: false,
        memoryMiB: 256,
        mounts,
      });
      if (!mountedRef.current || bootSeqRef.current !== seq) {
        await kernel.stop({ code: 0, reason: "cancelled" }).catch(() => undefined);
        return;
      }
      kernelRef.current = kernel;
      const decoder = new TextDecoder("utf-8", { fatal: false });
      unsubRef.current = kernel.onOutput((bytes) => {
        const text = decoder.decode(bytes, { stream: true });
        if (text.length > 0) {
          setOutput((prev) => trimOutput(prev + text));
        }
      });

      setStatus("running");
      try {
        setStats(kernel.stats());
      } catch {
        // ignore initial stats failure
      }

      void kernel.exited
        .then((res) => {
          if (kernelRef.current !== kernel) return;
          const tail = decoder.decode();
          if (tail.length > 0) {
            setOutput((prev) => trimOutput(prev + tail));
          }
          setExitInfo(`kind=${res.kind}, code=${res.code}${res.reason ? ` (${res.reason})` : ""}`);
          setStatus("exited");
          kernelRef.current = null;
          unsubRef.current?.();
          unsubRef.current = null;
        })
        .catch((err: unknown) => {
          if (kernelRef.current !== kernel) return;
          setErrorMsg(err instanceof Error ? err.message : "Sandbox exited unexpectedly");
          setStatus("error");
        });
    } catch (cause) {
      if (!mountedRef.current || bootSeqRef.current !== seq) return;
      setStatus("error");
      setErrorMsg(cause instanceof Error ? cause.message : "Failed to start unkernel Linux sandbox");
    }
  }, [support, cleanupKernel, locale, mountSpawnContext, principalLabel, canWrite, agentsCount]);

  const stopSandbox = useCallback(async () => {
    await cleanupKernel();
    setStatus(support?.ok ? "idle" : "unsupported");
  }, [cleanupKernel, support]);

  const sendInput = useCallback((text: string) => {
    const k = kernelRef.current;
    if (!k) return;
    k.write(text);
  }, []);

  const submitCommandLine = () => {
    if (!kernelRef.current) return;
    sendInput(`${commandInput}\n`);
    setCommandInput("");
  };

  const handleViewportKeyDown = (ev: React.KeyboardEvent<HTMLDivElement>) => {
    if (status !== "running" || !kernelRef.current) return;
    if (ev.metaKey || ev.altKey || ev.nativeEvent.isComposing) return;
    if (ev.ctrlKey) {
      const k = ev.key.toLowerCase();
      if (k === "c") {
        ev.preventDefault();
        sendInput("\u0003");
      } else if (k === "d") {
        ev.preventDefault();
        sendInput("\u0004");
      } else if (k === "l") {
        ev.preventDefault();
        sendInput("\u000c");
      }
      return;
    }
    if (ev.key === "Enter") {
      ev.preventDefault();
      sendInput("\r");
    } else if (ev.key === "Tab") {
      ev.preventDefault();
      sendInput("\t");
    } else if (ev.key === "Backspace") {
      ev.preventDefault();
      sendInput("\u007f");
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      sendInput("\u001b");
    } else if (ev.key === "ArrowUp") {
      ev.preventDefault();
      sendInput("\u001b[A");
    } else if (ev.key === "ArrowDown") {
      ev.preventDefault();
      sendInput("\u001b[B");
    } else if (ev.key === "ArrowRight") {
      ev.preventDefault();
      sendInput("\u001b[C");
    } else if (ev.key === "ArrowLeft") {
      ev.preventDefault();
      sendInput("\u001b[D");
    } else if (ev.key.length === 1) {
      ev.preventDefault();
      sendInput(ev.key);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3 sm:p-4">
      <Card>
        <CardHeader
          icon={<Cpu size={14} />}
          title={tr(locale, "浏览器内原生 Linux 7.x 沙箱（unkernel WebAssembly）", "Browser-Native Linux 7.x Sandbox (unkernel)")}
          meta={
            <div className="flex flex-wrap items-center gap-2">
              {status === "running" ? (
                <Badge tone="success" dot live>
                  Running
                </Badge>
              ) : status === "booting" ? (
                <Badge tone="info" dot live>
                  Booting
                </Badge>
              ) : status === "unsupported" ? (
                <Badge tone="warning">JSPI Unsupported</Badge>
              ) : status === "error" ? (
                <Badge tone="danger">Error</Badge>
              ) : (
                <Badge tone="neutral">{status}</Badge>
              )}
              {stats && (
                <span className="tnum font-mono text-micro text-faint-foreground">
                  {stats.cpus} CPU · compile {stats.compileMs}ms · up {Math.floor(stats.uptimeMs / 1000)}s
                </span>
              )}
            </div>
          }
        />

        <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-3 text-caption">
          <div className="flex flex-wrap items-center gap-3">
            <label className="inline-flex cursor-pointer items-center gap-1.5 text-muted-foreground">
              <input
                type="checkbox"
                checked={mountSpawnContext}
                onChange={(e) => setMountSpawnContext(e.target.checked)}
                disabled={status === "running" || status === "booting"}
                className="h-4 w-4 rounded border-surface-border accent-primary"
              />
              <span>
                {tr(locale, "只读挂载 /spawn/context.json", "Mount read-only /spawn/context.json")}
              </span>
            </label>
            <span className="font-mono text-micro text-faint-foreground">
              single-CPU · memory 256 MiB
            </span>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            {(status === "idle" || status === "exited" || status === "error") && (
              <button
                type="button"
                disabled={!support?.ok}
                onClick={() => void bootSandbox()}
                className={cx(
                  "inline-flex h-9 items-center gap-1.5 rounded-control bg-primary px-3 text-caption font-semibold text-primary-foreground shadow-btn transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                  FOCUS_RING,
                )}
              >
                <Play size={13} />
                {status === "exited" ? tr(locale, "重新启动", "Restart Linux") : tr(locale, "启动 Linux 沙箱", "Start Linux Sandbox")}
              </button>
            )}
            {(status === "running" || status === "booting") && (
              <>
                <Button onClick={() => void bootSandbox()} className="pointer-coarse:h-11">
                  <RotateCcw size={13} />
                  {tr(locale, "重启", "Reboot")}
                </Button>
                <Button onClick={() => void stopSandbox()} className="text-destructive pointer-coarse:h-11">
                  <Square size={13} />
                  {tr(locale, "停止", "Stop")}
                </Button>
              </>
            )}
          </div>
        </div>

        {support && !support.ok && (
          <div className="border-t border-surface-border bg-wait-tint px-4 py-3 text-caption text-warning">
            <div className="flex items-start gap-2">
              <AlertTriangle size={15} className="mt-0.5 shrink-0" />
              <div className="space-y-1">
                <div className="font-semibold">
                  {tr(
                    locale,
                    "当前浏览器内核暂不支持 WebAssembly JSPI（unkernel 运行前置条件）",
                    "Current browser lacks WebAssembly JSPI required by unkernel",
                  )}
                </div>
                <p className="font-mono text-micro">
                  wasm={String(support.wasm)} · bigint={String(support.bigint)} · jspi={String(support.jspi)}
                </p>
                {support.problems.length > 0 && (
                  <ul className="list-inside list-disc space-y-0.5">
                    {support.problems.map((p) => (
                      <li key={p}>{p}</li>
                    ))}
                  </ul>
                )}
                <p className="text-muted-foreground">
                  {tr(
                    locale,
                    "请使用 Chrome / Edge 137+（或开启 Experimental WebAssembly JSPI）打开控制台，即可直接零后端启动完整 Linux 7.x 沙箱。",
                    "Open in Chrome/Edge 137+ (or enable WebAssembly JSPI) to boot the full in-browser Linux 7.x sandbox.",
                  )}
                </p>
              </div>
            </div>
          </div>
        )}

        {errorMsg && (
          <div className="border-t border-surface-border bg-fail-tint px-4 py-2 text-caption text-destructive">
            {errorMsg}
          </div>
        )}

        {exitInfo && (
          <div className="border-t border-surface-border bg-inset px-4 py-2 font-mono text-caption text-muted-foreground">
            {tr(locale, "沙箱已退出：", "Sandbox exited: ")}
            {exitInfo}
          </div>
        )}
      </Card>

      {/* `data-theme="dark"` required alongside `bg-terminal` per contrast.mjs */}
      <section
        data-theme="dark"
        aria-label="unkernel Linux sandbox terminal"
        className="flex min-h-[280px] flex-1 flex-col overflow-hidden rounded-card border border-surface-border bg-terminal shadow-card"
      >
        <header className="flex min-h-9 shrink-0 flex-wrap items-center gap-2 border-b border-white/[0.08] px-3.5 py-1.5 text-caption">
          <Terminal size={13} className="text-muted-foreground" />
          <span className="font-mono font-medium text-foreground">
            unkernel /dev/console
          </span>
          <span className="font-mono text-micro text-muted-foreground">
            {terminalFocused
              ? tr(locale, "已聚焦（可直接键入）", "Focused (direct keyboard active)")
              : tr(locale, "点击终端画面可直接输入字符与 Ctrl+C", "Click terminal area to type directly")}
          </span>
          <button
            type="button"
            onClick={() => setOutput("")}
            className={cx(
              "ml-auto inline-flex items-center gap-1 rounded px-2 py-0.5 text-micro text-muted-foreground hover:bg-white/[0.08] hover:text-foreground",
              FOCUS_RING,
            )}
          >
            <Trash2 size={11} />
            {tr(locale, "清屏", "Clear")}
          </button>
        </header>

        <div
          ref={outputViewportRef}
          tabIndex={0}
          onFocus={() => setTerminalFocused(true)}
          onBlur={() => setTerminalFocused(false)}
          onKeyDown={handleViewportKeyDown}
          className={cx(
            "min-h-0 flex-1 overflow-auto px-3.5 py-3 font-mono text-[12px] leading-5 text-terminal-foreground outline-none",
            terminalFocused && "ring-1 ring-inset ring-ring",
          )}
        >
          {output.length === 0 ? (
            <EmptyState
              icon={<Terminal size={18} />}
              title={tr(locale, "Linux 沙箱尚未启动", "Linux sandbox not started")}
              hint={tr(
                locale,
                "点击上方「启动 Linux 沙箱」，即可在浏览器本地运行完整 BusyBox / Linux 7.x 会话。",
                "Click Start Linux Sandbox above to boot a local BusyBox / Linux 7.x environment.",
              )}
              className="py-10"
            />
          ) : (
            <pre className="whitespace-pre-wrap break-words font-[inherit]">{output}</pre>
          )}
        </div>
      </section>

      <Card className="p-3 sm:p-4">
        <div className="flex flex-wrap items-center gap-1.5 pb-2.5">
          {[
            { label: "Ctrl+C", seq: "\u0003" },
            { label: "Ctrl+D", seq: "\u0004" },
            { label: "Tab", seq: "\t" },
            { label: "uname -a", seq: "uname -a\n" },
            { label: "ls /spawn", seq: "ls -la /spawn && cat /spawn/context.json\n" },
          ].map((item) => (
            <button
              key={item.label}
              type="button"
              disabled={status !== "running"}
              onClick={() => sendInput(item.seq)}
              className={cx(
                "inline-flex h-8 items-center rounded-control border border-surface-border bg-inset px-2.5 font-mono text-caption text-foreground hover:bg-surface-hover disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
                FOCUS_RING,
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="text"
            value={commandInput}
            onChange={(e) => setCommandInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                submitCommandLine();
              }
            }}
            disabled={status !== "running"}
            placeholder={tr(locale, "输入 shell 命令后回车发送（如 ls -la /spawn）", "Type shell command and press Enter")}
            aria-label={tr(locale, "沙箱命令输入", "Sandbox command input")}
            className={cx(
              "h-9 min-w-52 flex-1 rounded-control border border-surface-border bg-surface px-2.5 font-mono text-caption text-foreground placeholder:text-faint-foreground disabled:opacity-50 pointer-coarse:h-11",
              FOCUS_RING,
            )}
          />
          <button
            type="button"
            disabled={status !== "running"}
            onClick={submitCommandLine}
            className={cx(
              "inline-flex h-9 items-center gap-1.5 rounded-control bg-primary px-3 text-caption font-semibold text-primary-foreground shadow-btn transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-45 pointer-coarse:h-11",
              FOCUS_RING,
            )}
          >
            {tr(locale, "执行命令", "Run")}
          </button>
        </div>
      </Card>
    </div>
  );
}
