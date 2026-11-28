import { Eye, EyeOff, LockKeyhole, Orbit } from "lucide-react";
import { useId, useState } from "react";
import type { FormEvent } from "react";

import { verifyAndApplyToken } from "../api";
import type { AuthPromptReason } from "../token";
import { ThemeSwitch } from "./Sidebar";
import { FOCUS_RING, cx } from "./ui";

interface TokenGateProps {
  reason: AuthPromptReason;
  problem: string | null;
  onAuthenticated: () => void;
}

const REASON_NOTICE: Record<AuthPromptReason, string | null> = {
  missing_token: null,
  rejected_token: "当前保存的 Token 无效或已过期，请重新输入。",
  invalid_token: "提供的 Token 格式无效，请输入有效的访问 Token。",
};

/**
 * Centered token-only login gate shown whenever `spawn serve` requires bearer auth.
 * Keeps a single Token field (no username), works with virtual keyboards on 16:9 tablets
 * and phones, and validates live against `GET /api/overview` before entering the shell.
 */
export function TokenGate({ reason, problem, onAuthenticated }: TokenGateProps) {
  const inputId = useId();
  const [token, setToken] = useState("");
  const [visible, setVisible] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const host = typeof window !== "undefined" ? window.location.host : "";
  const statusNote = error ?? REASON_NOTICE[reason] ?? problem;

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const verdict = await verifyAndApplyToken(token);
      if (verdict.ok) {
        setToken("");
        onAuthenticated();
        return;
      }
      setError(verdict.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="h-dvh w-full overflow-y-auto bg-page-canvas text-label text-foreground">
      <div className="flex min-h-full flex-col">
        <header className="flex shrink-0 items-center justify-end px-4 pt-3 sm:px-6 sm:pt-4">
          <ThemeSwitch />
        </header>

        <main className="flex flex-1 items-center justify-center px-4 py-4 sm:px-6">
          <section
            aria-labelledby="token-gate-title"
            className="w-full max-w-[400px] rounded-card bg-surface p-6 shadow-card sm:p-7"
          >
            <div className="flex items-center gap-3">
              <span className="grid size-9 shrink-0 place-items-center rounded-control border border-border-strong bg-inset text-foreground">
                <Orbit size={18} strokeWidth={2} />
              </span>
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-body font-semibold tracking-tight text-foreground">
                    Spawn
                  </span>
                  {host && (
                    <span className="truncate rounded-chip bg-muted px-1.5 py-0.5 font-mono text-micro text-muted-foreground">
                      {host}
                    </span>
                  )}
                </div>
                <p className="mt-0.5 text-caption text-muted-foreground">
                  仅需唯一 Token，无需用户名
                </p>
              </div>
            </div>

            <h1
              id="token-gate-title"
              className="mt-5 text-title font-semibold tracking-tight text-foreground"
            >
              输入 Token 登录
            </h1>

            <form onSubmit={(event) => void handleSubmit(event)} className="mt-4 space-y-4">
              <div>
                <label
                  htmlFor={inputId}
                  className="mb-1.5 flex items-center gap-1.5 text-caption font-medium text-muted-foreground"
                >
                  <LockKeyhole size={12} aria-hidden="true" />
                  访问令牌 (Token)
                </label>
                <div className="relative flex items-center rounded-control bg-inset shadow-inset-field ring-1 ring-inset ring-line transition-[background-color,box-shadow] focus-within:bg-surface focus-within:shadow-raised focus-within:ring-ring">
                  <input
                    id={inputId}
                    type={visible ? "text" : "password"}
                    value={token}
                    disabled={busy}
                    autoFocus
                    autoComplete="current-password"
                    autoCapitalize="off"
                    autoCorrect="off"
                    spellCheck={false}
                    placeholder="粘贴 SPAWN_SERVE_TOKEN…"
                    onChange={(event) => {
                      setToken(event.target.value);
                      if (error) setError(null);
                    }}
                    className="h-11 w-full min-w-0 bg-transparent pr-10 pl-3 font-mono text-body text-foreground placeholder:font-sans placeholder:text-faint-foreground focus:outline-none disabled:cursor-not-allowed"
                  />
                  <button
                    type="button"
                    onClick={() => setVisible((current) => !current)}
                    aria-label={visible ? "隐藏 Token" : "显示 Token"}
                    title={visible ? "隐藏 Token" : "显示 Token"}
                    className={cx(
                      "absolute right-1.5 grid size-8 place-items-center rounded-md text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                      FOCUS_RING,
                    )}
                  >
                    {visible ? <EyeOff size={15} /> : <Eye size={15} />}
                  </button>
                </div>
              </div>

              {statusNote && (
                <p
                  role={error ? "alert" : "status"}
                  className={cx(
                    "rounded-control px-3 py-2 text-caption",
                    error || reason === "rejected_token"
                      ? "bg-fail-tint text-destructive"
                      : "bg-wait-tint text-warning",
                  )}
                >
                  {statusNote}
                </p>
              )}

              <button
                type="submit"
                disabled={busy}
                className={cx(
                  "inline-flex h-11 w-full items-center justify-center rounded-control bg-primary px-4 text-body font-semibold text-primary-foreground shadow-btn transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50",
                  FOCUS_RING,
                )}
              >
                {busy ? "正在验证 Token…" : "进入控制台"}
              </button>
            </form>

            <p className="mt-4 text-caption leading-relaxed text-faint-foreground">
              可在服务端 <code className="font-mono text-muted-foreground">spawn.env</code> 查看{" "}
              <code className="font-mono text-muted-foreground">SPAWN_SERVE_TOKEN</code>，或通过带{" "}
              <code className="font-mono text-muted-foreground">/?token=...</code> 的直达链接自动登录。
            </p>
          </section>
        </main>
      </div>
    </div>
  );
}
