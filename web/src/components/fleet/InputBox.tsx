import {
  Ban,
  CircleAlert,
  CircleCheck,
  Clock,
  Hourglass,
  LoaderCircle,
  SendHorizontal,
} from "lucide-react";
import type { ReactNode } from "react";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";

import { postInput } from "../../api";
import { fetchCommands } from "../../fleet";
import { clockTime } from "../../format";
import type { CommandState, Progress, Sent, Writable } from "../../input";
import {
  newKey,
  probeVerdict,
  progress,
  sendAnswer,
  settled,
  textProblem,
  withSent,
} from "../../input";
import { cx } from "../ui";

/** Tallest the box grows, in pixels, before it scrolls. */
const MAX_HEIGHT = 200;

/**
 * A person's input to one session (`POST /api/session/<id>/input`): one
 * line that grows, Enter sends, Shift+Enter starts a new line. Under it,
 * what this page sent there and what became of each: queued, waiting for
 * the agent's turn, typed, or why not (from the session's commands). It
 * asks the server first (the probe) and stays disabled, with the reason,
 * when this page may not type. A view that already polls the session's
 * commands passes its reading (`commands`), so the box and that view never
 * disagree; without it the box reads them itself while a message is open.
 */
export function InputBox({
  sessionId,
  to,
  stopped = false,
  commands: shared,
  className,
}: {
  sessionId: string;
  /** Who reads it, in words: "the supervisor", an agent's id. */
  to: string;
  /** The session runs no execution: an input to it fails at once. */
  stopped?: boolean;
  /** The session's commands as the enclosing view last read them; null
   * before its first reading. Absent: the box reads them itself. */
  commands?: readonly CommandState[] | null;
  className?: string;
}) {
  const writable = useWritable(sessionId);
  const [refusedBy, setRefusedBy] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [sent, setSent] = useState<Sent[]>([]);
  // A message not taken keeps its key: sent again unchanged, it types once.
  const [retry, setRetry] = useState<{ text: string; key: string } | null>(null);
  const [sending, setSending] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const own = useCommandsWhile(sessionId, sent, shared === undefined);
  const commands = shared === undefined ? own : shared;
  const area = useRef<HTMLTextAreaElement>(null);
  const id = useId();

  useLayoutEffect(() => {
    const element = area.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, MAX_HEIGHT)}px`;
  }, [text]);

  const state: Writable = refusedBy ? { state: "no", reason: refusedBy } : writable;
  const ready = state.state === "yes";

  const send = async () => {
    if (sending || !ready) return;
    const body = text;
    const wrong = textProblem(body);
    setProblem(wrong);
    if (wrong) return;
    const key = retry?.text === body ? retry.key : newKey();
    const entry: Sent = { key, text: body, at_ms: Date.now(), command_id: null, error: null };
    setSent((list) => withSent(list, entry));
    setSending(true);
    let answer = await postInput(sessionId, { text: body, key });
    if (answer.status === null) {
      // Not reached: once more, with the same key, so it types once.
      await new Promise((resolve) => window.setTimeout(resolve, 1_000));
      answer = await postInput(sessionId, { text: body, key });
    }
    const verdict = sendAnswer(answer.status, answer.body);
    if (verdict.ok) {
      setSent((list) => withSent(list, { ...entry, command_id: verdict.command_id }));
      setText((current) => (current === body ? "" : current));
      setRetry(null);
    } else {
      setSent((list) => withSent(list, { ...entry, error: verdict.error }));
      setRetry({ text: body, key });
      if (verdict.status === 401 || verdict.status === 403) setRefusedBy(verdict.error);
    }
    setSending(false);
  };

  const placeholder =
    state.state === "checking"
      ? "Checking whether this page may type…"
      : ready
        ? `Type to ${to}… Enter sends, Shift+Enter starts a new line`
        : "Read-only here";

  return (
    <div className={cx("min-w-0", className)}>
      <div className="flex items-end gap-2">
        <textarea
          id={id}
          ref={area}
          rows={1}
          value={text}
          disabled={!ready || sending}
          aria-label={`Type to ${to}`}
          placeholder={placeholder}
          onChange={(event) => {
            setText(event.target.value);
            if (problem) setProblem(null);
          }}
          onKeyDown={(event) => {
            // Enter sends; Shift+Enter is a new line; an IME's Enter
            // (composing Chinese, say) is the IME's.
            if (
              event.key === "Enter" &&
              !event.shiftKey &&
              !event.nativeEvent.isComposing &&
              event.keyCode !== 229
            ) {
              event.preventDefault();
              void send();
            }
          }}
          style={{ maxHeight: MAX_HEIGHT }}
          className="min-h-8 min-w-0 flex-1 resize-none overflow-y-auto rounded-md border border-surface-border bg-background px-3 py-1.5 text-label leading-5 text-foreground placeholder:text-faint-foreground focus-visible:border-border-strong focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-60"
        />
        <button
          type="button"
          onClick={() => void send()}
          disabled={!ready || sending || text.trim() === ""}
          title={ready ? `Send to ${to} (Enter)` : undefined}
          className="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-md border border-border-strong bg-surface-raised px-3 text-caption font-medium text-foreground transition-colors hover:bg-surface-hover focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-50"
        >
          {sending ? (
            <LoaderCircle size={13} className="animate-spin" />
          ) : (
            <SendHorizontal size={13} />
          )}
          Send
        </button>
      </div>
      {state.state === "no" && (
        <p className="mt-1 flex items-start gap-1.5 text-caption text-muted-foreground">
          <Ban size={12} className="mt-0.5 shrink-0" />
          <span>{state.reason}</span>
        </p>
      )}
      {problem && <p className="mt-1 text-caption text-destructive">{problem}</p>}
      {ready && stopped && (
        <p className="mt-1 text-caption text-faint-foreground">
          {to} runs no execution now: an input to it fails at once.
        </p>
      )}
      {sent.length > 0 && (
        <ul className="mt-1.5 space-y-1" aria-label={`Sent to ${to}`}>
          {sent.map((entry) => (
            <SentRow key={entry.key} entry={entry} state={progress(entry, commands)} />
          ))}
        </ul>
      )}
    </div>
  );
}

/** Whether this page may type into `sessionId`: the probe's answer, asked
 * again every 30 s while it is not yes. */
export function useWritable(sessionId: string): Writable {
  const [writable, setWritable] = useState<Writable>({ state: "checking" });
  useEffect(() => {
    const controller = new AbortController();
    let timer: number | undefined;
    const ask = async () => {
      try {
        const answer = await postInput(sessionId, { probe: true }, controller.signal);
        const verdict = probeVerdict(answer.status, answer.body);
        setWritable(verdict);
        if (verdict.state !== "yes") timer = window.setTimeout(() => void ask(), 30_000);
      } catch {
        // Aborted: the box is gone or names another session.
      }
    };
    setWritable({ state: "checking" });
    void ask();
    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [sessionId]);
  return writable;
}

/** The session's commands, read every 2 s while a message in `sent` is
 * not settled and `enabled`; the last reading otherwise. */
function useCommandsWhile(
  sessionId: string,
  sent: readonly Sent[],
  enabled: boolean,
): CommandState[] | null {
  const [commands, setCommands] = useState<CommandState[] | null>(null);
  const open = enabled && sent.some((entry) => !settled(progress(entry, commands)));
  useEffect(() => {
    if (!open) return;
    const controller = new AbortController();
    const read = async () => {
      try {
        setCommands(await fetchCommands(sessionId, controller.signal, 20));
      } catch {
        // Keep the last reading; the next one may succeed.
      }
    };
    void read();
    const timer = window.setInterval(() => void read(), 2_000);
    return () => {
      controller.abort();
      window.clearInterval(timer);
    };
  }, [open, sessionId]);
  return commands;
}

function SentRow({ entry, state }: { entry: Sent; state: Progress }) {
  const [icon, label, tone]: [ReactNode, string, string] = (() => {
    switch (state.stage) {
      case "sending":
        return [<LoaderCircle size={12} className="animate-spin" />, "Sending", "text-muted-foreground"];
      case "queued":
        return [<Clock size={12} />, "Sent", "text-muted-foreground"];
      case "waiting":
        return [<Hourglass size={12} />, "Waiting for its turn", "text-warning"];
      case "typed":
        return [
          <CircleCheck size={12} />,
          state.at_ms ? `Typed ${clockTime(state.at_ms)}` : "Typed",
          "text-success",
        ];
      case "failed":
        return [<CircleAlert size={12} />, "Not typed", "text-destructive"];
      case "refused":
        return [<CircleAlert size={12} />, "Not sent", "text-destructive"];
    }
  })();
  const error = state.stage === "failed" || state.stage === "refused" ? state.error : null;
  const id = "command_id" in state ? state.command_id : null;
  return (
    <li className="min-w-0 text-caption">
      <div className="flex min-w-0 items-baseline gap-2">
        <span
          className={cx("inline-flex shrink-0 items-center gap-1 self-center font-medium", tone)}
          title={
            state.stage === "waiting"
              ? "The agent is in a turn: the Bridge types it when the turn ends"
              : state.stage === "queued"
                ? "Taken; not handled yet"
                : undefined
          }
        >
          {icon}
          {label}
        </span>
        <span className="min-w-0 truncate text-muted-foreground" title={entry.text}>
          {entry.text.replace(/\s+/g, " ")}
        </span>
        {id !== null && (
          <span className="tnum ml-auto shrink-0 font-mono text-micro text-faint-foreground">
            #{id}
          </span>
        )}
      </div>
      {error && (
        <p className="mt-0.5 break-words text-destructive">
          {error}
          {state.stage === "refused" &&
            error.startsWith("Cannot reach") &&
            " Send it again: unchanged, it keeps its key and types once."}
        </p>
      )}
    </li>
  );
}
