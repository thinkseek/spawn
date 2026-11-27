import { CircleAlert, CircleCheck, Clock, CloudOff, LoaderCircle, SendHorizontal, Trash2 } from "lucide-react";
import { useCallback, useEffect, useId, useRef, useState } from "react";

import { live, postMaster } from "../../api";
import { newKey, textProblem } from "../../input";
import type { MasterMode } from "../../master";
import { MASTER_MODES, MODE_HELP, expired, isMode, masterBody, masterVerdict } from "../../master";
import type { OutboxEntry, SendOutcome } from "../../outbox";
import { Outbox, openOutboxStore } from "../../outbox";
import { KeyCap, PixelGridLoader, ShimmerLabel } from "../BeautifulPrimitives";
import { cx } from "../ui";

/** The page's one outbox (outbox.ts): IndexedDB when it opens, else memory. */
let outboxOpening: Promise<Outbox> | null = null;
function outbox(): Promise<Outbox> {
  outboxOpening ??= openOutboxStore(typeof indexedDB === "undefined" ? null : indexedDB).then(
    (store) => new Outbox(store),
  );
  return outboxOpening;
}

/** One send of an outbox entry: its own key, as body and header. */
async function sendEntry(entry: OutboxEntry): Promise<SendOutcome & { detail?: string }> {
  const mode: MasterMode = isMode(entry.mode) ? entry.mode : "auto";
  const reply = await postMaster(masterBody(entry.text, mode, entry.key));
  const verdict = masterVerdict(reply.status, reply.body);
  if (verdict.state === "sent") {
    const how = verdict.replayed ? "already taken" : verdict.woke ? "the supervisor was woken" : "taken";
    return { state: "sent", detail: `${how}, #${verdict.command_id}` };
  }
  return verdict.state === "retry" ? { state: "retry", error: verdict.error } : { state: "refused", error: verdict.error };
}

const failure = (cause: unknown) => (cause instanceof Error ? cause.message : "something failed");

type Shown = { key: string; text: string; state: "sent" | "refused"; detail: string };
type Writable = { state: "checking" | "yes" } | { state: "no"; reason: string };

/** How often a waiting outbox is tried again while the page is open. */
const RETRY_MS = 15_000;

/**
 * Master Control (issue #91 P1b, `POST /api/master/message`): a message to
 * the supervisor in one of four modes (auto, steer, queue, restart). A
 * stopped supervisor is woken first, never refused. Each message is written
 * to the outbox with its idempotency key before it is sent; one the
 * network dropped waits there (in IndexedDB, across reloads; in memory
 * when IndexedDB is missing or fails, which it says) and is sent again,
 * with the same key, when the browser is back online, the page is shown
 * again, the live stream reconnects, or every 15 s: the server types it
 * once however often it arrives. A queued steer or restart older than five
 * minutes is held instead (master.ts `expired`): the person sends it
 * anyway or discards it, as they may any waiting message. Whether this
 * page may write (the probe, `{"probe": true}`) is asked at mount and
 * again when the browser is back online, the page shows again or the
 * stream reconnects, so a tab opened offline recovers by itself.
 */
export function MasterComposer() {
  const [text, setText] = useState("");
  const [mode, setMode] = useState<MasterMode>("auto");
  const [writable, setWritable] = useState<Writable>({ state: "checking" });
  const [waiting, setWaiting] = useState<OutboxEntry[]>([]);
  const [shown, setShown] = useState<Shown[]>([]);
  const [durable, setDurable] = useState(true);
  const [problem, setProblem] = useState<string | null>(null);
  const [trouble, setTrouble] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const id = useId();
  const mounted = useRef(true);

  /** Refresh what the outbox holds and how it keeps it. */
  const showBox = useCallback(async (box: Outbox) => {
    const pending = await box.pending();
    if (!mounted.current) return;
    setWaiting(pending);
    setDurable(box.store.durable);
    if (box.degraded) setTrouble(box.degraded);
  }, []);

  const flush = useCallback(async () => {
    try {
      const box = await outbox();
      const details = new Map<string, string>();
      const result = await box.flush(
        async (entry) => {
          const outcome = await sendEntry(entry);
          if (outcome.detail) details.set(entry.key, outcome.detail);
          return outcome;
        },
        (entry) => expired(entry.mode, entry.created_at_ms, Date.now()),
      );
      if (!mounted.current) return;
      const done: Shown[] = [
        ...result.sent.map((entry) => ({
          key: entry.key,
          text: entry.text,
          state: "sent" as const,
          detail: details.get(entry.key) ?? "taken",
        })),
        ...result.refused.map(({ entry, error }) => ({
          key: entry.key,
          text: entry.text,
          state: "refused" as const,
          detail: error,
        })),
      ];
      if (done.length) {
        setShown((list) => [...done, ...list.filter((row) => !done.some((d) => d.key === row.key))].slice(0, 5));
      }
      await showBox(box);
    } catch (cause) {
      if (mounted.current) setTrouble(`The outbox failed: ${failure(cause)}`);
    }
  }, [showBox]);

  const probe = useCallback(async (signal: AbortSignal) => {
    try {
      const reply = await postMaster({ probe: true }, signal);
      const body = reply.body as { writable?: unknown; error?: unknown } | null;
      if (!mounted.current) return;
      if (reply.status === 200 && body?.writable === true) setWritable({ state: "yes" });
      else
        setWritable({
          state: "no",
          reason:
            typeof body?.error === "string"
              ? body.error
              : reply.status === null
                ? "Cannot reach the server; asked again when the network is back."
                : `HTTP ${reply.status}`,
        });
    } catch {
      // Aborted: the composer is gone.
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    const controller = new AbortController();
    void probe(controller.signal);
    void outbox()
      .then(async (box) => {
        await showBox(box);
        await flush();
      })
      .catch((cause: unknown) => {
        if (mounted.current) setTrouble(`The outbox could not be opened: ${failure(cause)}`);
      });
    // Back online, shown again, reconnected: may this page write now, and
    // send what waits.
    const again = () => {
      void probe(controller.signal);
      void flush();
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") again();
    };
    window.addEventListener("online", again);
    document.addEventListener("visibilitychange", onVisible);
    const unsubscribe = live.subscribe((signal) => {
      if (signal.kind === "ready") again();
    });
    const timer = window.setInterval(() => void flush(), RETRY_MS);
    return () => {
      mounted.current = false;
      controller.abort();
      window.removeEventListener("online", again);
      document.removeEventListener("visibilitychange", onVisible);
      unsubscribe();
      window.clearInterval(timer);
    };
  }, [flush, probe, showBox]);

  const send = async () => {
    if (busy || writable.state !== "yes") return;
    const wrong = textProblem(text);
    setProblem(wrong);
    if (wrong) return;
    setBusy(true);
    try {
      const box = await outbox();
      // Kept first: if this fails, the text stays in the box, unsent.
      await box.add(newKey(), text, mode);
      setText("");
      setTrouble(box.degraded);
      await showBox(box);
      await flush();
    } catch (cause) {
      setTrouble(`Not sent, and not kept: ${failure(cause)}. The text is still in the box.`);
    } finally {
      if (mounted.current) setBusy(false);
    }
  };

  const discard = async (key: string) => {
    try {
      const box = await outbox();
      await box.discard(key);
      await showBox(box);
    } catch (cause) {
      setTrouble(`Could not discard it: ${failure(cause)}`);
    }
  };

  const sendAnyway = async (key: string) => {
    try {
      const box = await outbox();
      await box.renew(key);
      await flush();
    } catch (cause) {
      setTrouble(`Could not send it: ${failure(cause)}`);
    }
  };

  const ready = writable.state === "yes";
  const now = Date.now();
  return (
    <section aria-label="Master Control" className="rounded-card bg-surface px-4 py-3 shadow-card">
      <div className="mb-2 flex flex-wrap items-center gap-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        <label htmlFor={id}>To the supervisor</label>
        {writable.state === "checking" && (
          <span className="inline-flex items-center gap-1.5 normal-case tracking-normal text-muted-foreground">
            <PixelGridLoader tone="accent" size="xs" />
            <ShimmerLabel>Probing access…</ShimmerLabel>
          </span>
        )}
        {ready && (
          <span className="ml-auto rounded-chip bg-inset px-1.5 py-0.5 text-micro font-medium normal-case tracking-normal text-muted-foreground shadow-hairline">
            Master prompt
          </span>
        )}
      </div>

      {/* Beautiful UI 08 Prompt Bar: unified inset composer + integrated mode & send toolbar */}
      <div
        className={cx(
          "rounded-control bg-inset shadow-inset-field ring-1 ring-inset ring-line transition-[background-color,box-shadow] focus-within:bg-surface focus-within:shadow-raised focus-within:ring-ring",
          !ready && "opacity-75",
        )}
      >
        <textarea
          id={id}
          rows={2}
          value={text}
          disabled={!ready}
          placeholder={
            writable.state === "checking"
              ? "Checking whether this page may write…"
              : ready
                ? `Message the supervisor (${mode}): ${MODE_HELP[mode]}`
                : "Read-only here"
          }
          onChange={(event) => {
            setText(event.target.value);
            if (problem) setProblem(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter" && (event.ctrlKey || event.metaKey) && !event.nativeEvent.isComposing) {
              event.preventDefault();
              void send();
            }
          }}
          className="block min-h-12 w-full resize-y bg-transparent px-3 pt-2.5 pb-1.5 text-label leading-5 text-foreground placeholder:text-faint-foreground focus:outline-none disabled:cursor-not-allowed"
        />

        <div className="flex flex-wrap items-center gap-2 border-t border-surface-border px-2.5 py-1.5">
          <div role="radiogroup" aria-label="Mode" className="flex flex-wrap items-center gap-1">
            {MASTER_MODES.map((option) => (
              <button
                key={option}
                type="button"
                role="radio"
                aria-checked={mode === option}
                title={MODE_HELP[option]}
                onClick={() => setMode(option)}
                className={cx(
                  "rounded-chip px-2 py-0.5 text-caption font-medium transition-[color,background-color,box-shadow] focus-visible:outline-2 focus-visible:outline-ring",
                  mode === option
                    ? option === "restart"
                      ? "bg-surface text-destructive shadow-btn"
                      : "bg-surface text-foreground shadow-btn"
                    : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                )}
              >
                {option}
              </button>
            ))}
          </div>

          <div className="ml-auto flex items-center gap-2">
            <KeyCap className="hidden sm:inline-flex">Ctrl↵</KeyCap>
            <button
              type="button"
              onClick={() => void send()}
              disabled={!ready || busy || text.trim() === ""}
              className={cx(
                "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-chip px-2.5 text-caption font-medium transition-[color,background-color,box-shadow,opacity] focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-45",
                ready && text.trim() !== ""
                  ? "bg-primary text-primary-foreground shadow-btn hover:opacity-90"
                  : "bg-surface text-muted-foreground shadow-hairline",
              )}
            >
              {busy ? <LoaderCircle size={13} className="animate-spin" /> : <SendHorizontal size={13} />}
              Send
            </button>
          </div>
        </div>
      </div>
      {writable.state === "no" && <p className="mt-1.5 text-caption text-muted-foreground">{writable.reason}</p>}
      {problem && <p className="mt-1.5 text-caption text-destructive">{problem}</p>}
      {trouble && (
        <p role="alert" className="mt-1.5 text-caption text-destructive">
          {trouble}
        </p>
      )}
      {(waiting.length > 0 || shown.length > 0) && (
        <ul className="mt-2 space-y-1.5 text-caption" aria-label="Messages to the supervisor">
          {waiting.map((entry) => {
            const held = expired(entry.mode, entry.created_at_ms, now);
            return (
              <li
                key={entry.key}
                className={cx(
                  "flex min-w-0 items-center gap-2 rounded-control bg-inset px-2.5 py-1.5 shadow-hairline",
                  held ? "text-destructive" : "text-warning",
                )}
              >
                {held ? <Clock size={12} className="shrink-0" /> : <CloudOff size={12} className="shrink-0" />}
                <span className="shrink-0 font-medium">
                  {held ? `Expired ${entry.mode}: not sent by itself` : "Waiting to send"}
                </span>
                <span className="min-w-0 truncate text-muted-foreground" title={entry.text}>
                  {entry.text.replace(/\s+/g, " ")}
                </span>
                {entry.last_error && !held && (
                  <span className="shrink-0 truncate text-faint-foreground" title={entry.last_error}>
                    tried {entry.attempts}×{durable ? "" : " (kept in this tab only)"}
                  </span>
                )}
                <span className="ml-auto flex shrink-0 gap-1">
                  {held && (
                    <button
                      type="button"
                      disabled={!ready}
                      onClick={() => void sendAnyway(entry.key)}
                      title={`Send this ${entry.mode} now, as if written now`}
                      className="rounded-chip bg-surface px-1.5 py-0.5 text-micro font-medium text-foreground shadow-btn hover:bg-surface-hover disabled:opacity-50"
                    >
                      Send anyway
                    </button>
                  )}
                  <button
                    type="button"
                    onClick={() => void discard(entry.key)}
                    title="Discard: never send this message"
                    aria-label="Discard"
                    className="inline-flex items-center gap-0.5 rounded-chip px-1.5 py-0.5 text-micro font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground"
                  >
                    <Trash2 size={11} aria-hidden="true" />
                    Discard
                  </button>
                </span>
              </li>
            );
          })}
          {shown.map((row) => (
            <li
              key={row.key}
              className={cx(
                "flex min-w-0 items-baseline gap-2 rounded-control bg-inset px-2.5 py-1.5 shadow-hairline",
                row.state === "sent" ? "text-success" : "text-destructive",
              )}
            >
              {row.state === "sent" ? (
                <CircleCheck size={12} className="shrink-0 self-center" />
              ) : (
                <CircleAlert size={12} className="shrink-0 self-center" />
              )}
              <span className="shrink-0 font-medium">{row.state === "sent" ? "Sent" : "Not sent"}</span>
              <span className="min-w-0 truncate text-muted-foreground" title={row.text}>
                {row.text.replace(/\s+/g, " ")}
              </span>
              <span className="ml-auto shrink-0 truncate text-faint-foreground" title={row.detail}>
                {row.detail}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
