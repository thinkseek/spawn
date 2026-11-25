import { CircleAlert, CircleCheck, LoaderCircle, ShieldQuestion } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { fetchApprovals, postApprovalAct, postResume, usePoll } from "../../api";
import type { Approval, ApprovalOption } from "../../approvals";
import { actBody, actVerdict, kindText, probeWritable, resumeVerdict } from "../../approvals";
import { relativeTime } from "../../format";
import { newKey } from "../../input";
import { cx } from "../ui";

type Probe = { state: "checking" } | { state: "yes" } | { state: "no"; reason: string };

/** Whether this page may answer each session's prompts (and each halted
 * mission's resume): `{"probe": true}` once per target, as the input box
 * asks, before any button is enabled. A target that says no is asked
 * again after 30 s. */
function useProbes(items: readonly Approval[]): Map<string, Probe> {
  const [probes, setProbes] = useState<Map<string, Probe>>(() => new Map());
  const probesRef = useRef(probes);
  probesRef.current = probes;
  // The set of targets as one string: a new reading of the same queue
  // asks nothing again.
  const targetKey = useMemo(
    () =>
      [...new Set(items.map((item) => (item.resume ? `mission:${item.resume.mission}` : item.session_id)))]
        .filter((target): target is string => target !== null)
        .sort()
        .join("\n"),
    [items],
  );
  // Targets asked (answered or in flight), and those answered.
  const asked = useRef(new Set<string>());
  const answered = useRef(new Set<string>());
  useEffect(() => {
    const targets = targetKey === "" ? [] : targetKey.split("\n");
    const controller = new AbortController();
    const timers: number[] = [];
    const ask = async (target: string) => {
      setProbes((map) => new Map(map).set(target, { state: "checking" }));
      try {
        const mission = /^mission:(\d+)$/.exec(target);
        const answer = mission
          ? await postResume(Number(mission[1]), { probe: true }, controller.signal)
          : await postApprovalAct(target, { probe: true }, controller.signal);
        const verdict = probeWritable(answer.status, answer.body);
        answered.current.add(target);
        setProbes((map) =>
          new Map(map).set(target, verdict.writable ? { state: "yes" } : { state: "no", reason: verdict.reason ?? "" }),
        );
        if (!verdict.writable) timers.push(window.setTimeout(() => void ask(target), 30_000));
      } catch {
        // Aborted: the queue is gone or lists other targets.
      }
    };
    for (const target of targets) {
      if (asked.current.has(target)) continue;
      asked.current.add(target);
      void ask(target);
    }
    return () => {
      controller.abort();
      for (const timer of timers) window.clearTimeout(timer);
      // A probe cut short (or a refusal's retry) is asked again next time.
      for (const target of targets) {
        if (!answered.current.has(target) || probesRef.current.get(target)?.state !== "yes") {
          asked.current.delete(target);
          answered.current.delete(target);
        }
      }
    };
  }, [targetKey]);
  return probes;
}

type Outcome = { state: "sending" } | { state: "done"; text: string } | { state: "failed"; error: string };

/** How long an answered item stays hidden while the queue still lists it
 * (the server's next reading may not have the pane's change yet); past
 * it, it shows again, marked sent but still showing. */
const HIDE_MS = 12_000;

/**
 * The approvals queue (issue #91 P1b, `GET /api/approvals`): every prompt a
 * person must answer on a live pane, with one button per answer (a
 * numbered choice's digits, Allow / Always allow / Deny, yes / no, Enter),
 * sessions whose judge says they wait for a person (answered by typing on
 * the agent's page), and halted missions with their Resume. A click sends
 * `POST /api/approvals/<session_id>/act` with the item's `approval_id` and
 * an idempotency key (body and `Idempotency-Key` header), sent once more
 * with the same key when the server was not reached; a 409 means the
 * prompt changed and the queue is read again. An answered item is hidden
 * for at most 12 s; listed after that, it shows again as "Sent, still
 * showing". It reads again on every live event, polling slowly besides;
 * nothing shows while nothing waits.
 */
export function ApprovalsQueue({ onOpenAgent }: { onOpenAgent?: (sessionId: string) => void }) {
  const { data, error } = usePoll("approvals", fetchApprovals, 10_000, { live: ["approvals"] });
  // Each answered item and when its answer was taken.
  const [answered, setAnswered] = useState<Map<string, number>>(() => new Map());
  const [clock, setClock] = useState(() => Date.now());
  const [outcomes, setOutcomes] = useState<Map<string, Outcome>>(() => new Map());
  // One key per item and answer: a click again (after no answer) sends the
  // same key, so the server presses it once.
  const keys = useRef(new Map<string, string>());
  const items = useMemo(
    () =>
      (data?.items ?? []).filter((item) => {
        const at = answered.get(item.id);
        return at === undefined || clock - at >= HIDE_MS;
      }),
    [data, answered, clock],
  );
  // Show a hidden item again when its time is up.
  useEffect(() => {
    if (answered.size === 0) return;
    const next = Math.min(...[...answered.values()].map((at) => at + HIDE_MS));
    if (next <= clock) return;
    const timer = window.setTimeout(() => setClock(Date.now()), next - Date.now() + 50);
    return () => window.clearTimeout(timer);
  }, [answered, clock]);
  const probes = useProbes(items);
  // A new reading of the queue is the truth: forget what it no longer lists.
  useEffect(() => {
    if (!data) return;
    const listed = new Set(data.items.map((item) => item.id));
    setAnswered((map) => new Map([...map].filter(([id]) => listed.has(id))));
  }, [data]);

  if (!error && items.length === 0) return null;

  const setOutcome = (id: string, outcome: Outcome) => setOutcomes((map) => new Map(map).set(id, outcome));

  const answer = async (item: Approval, choice: ApprovalOption) => {
    if (choice.danger && !window.confirm(`${choice.label}?\n\n${item.summary ?? item.label}`)) return;
    const slot = `${item.id}\n${choice.id}`;
    const key = keys.current.get(slot) ?? newKey();
    keys.current.set(slot, key);
    setOutcome(item.id, { state: "sending" });
    const send = async () => {
      if (item.resume) {
        return postResume(item.resume.mission, { halt: item.resume.halt, key });
      }
      const body = actBody(item, choice, key);
      if (!body || item.session_id === null) return { status: 400, body: { error: "This answer cannot be sent from here." } };
      return postApprovalAct(item.session_id, body);
    };
    let reply = await send();
    if (reply.status === null) {
      await new Promise((resolve) => window.setTimeout(resolve, 1_000));
      reply = await send();
    }
    const verdict = item.resume ? resumeVerdict(reply.status, reply.body) : actVerdict(reply.status, reply.body);
    if (verdict.ok) {
      keys.current.delete(slot);
      setOutcome(item.id, { state: "done", text: `${choice.label}: sent` });
      const at = Date.now();
      setAnswered((map) => new Map(map).set(item.id, at));
      setClock(at);
    } else {
      // A key the server holds for another request is not used again.
      if (!verdict.retry) keys.current.delete(slot);
      setOutcome(item.id, {
        state: "failed",
        error: verdict.stale ? `${verdict.error} (the queue is read again on the next event)` : verdict.error,
      });
    }
  };

  return (
    <section aria-label="Approvals" className="rounded-card bg-surface px-4 py-3 shadow-card">
      <div className="mb-2 flex flex-wrap items-baseline gap-x-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        <ShieldQuestion size={12} aria-hidden="true" className="self-center" />
        To answer
        <span className="tnum normal-case tracking-normal">{items.length}</span>
        {error && (
          <span role="status" className="normal-case tracking-normal text-destructive">
            {data ? `not updated: ${error}` : `could not be read: ${error}`}
          </span>
        )}
      </div>
      <ul className="flex flex-col gap-2">
        {items.map((item) => {
          const target = item.resume ? `mission:${item.resume.mission}` : item.session_id;
          const probe: Probe = target ? (probes.get(target) ?? { state: "checking" }) : { state: "no", reason: "" };
          const answeredAt = answered.get(item.id);
          const lingering = answeredAt !== undefined && clock - answeredAt >= HIDE_MS;
          const outcome: Outcome | undefined = lingering
            ? { state: "done", text: "Sent, still showing: the pane may not have changed yet, or the answer did not take" }
            : outcomes.get(item.id);
          const busy = outcome?.state === "sending";
          return (
            <li key={item.id} className="min-w-0 rounded-md border border-surface-border px-3 py-2">
              <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 text-caption">
                <span className="rounded-sm bg-muted px-1 text-micro font-medium uppercase tracking-[0.04em] text-muted-foreground">
                  {kindText(item.kind)}
                </span>
                {item.session_id && (
                  <button
                    type="button"
                    className="truncate font-medium text-foreground hover:underline"
                    onClick={() => item.session_id && onOpenAgent?.(item.session_id)}
                    title={item.session_id}
                  >
                    {item.agent_id ?? item.session_id}
                  </button>
                )}
                <span className="min-w-0 truncate text-muted-foreground">{item.label}</span>
                {item.detected_at_ms !== null && (
                  <span className="tnum ml-auto shrink-0 text-micro text-faint-foreground">
                    {relativeTime(item.detected_at_ms)}
                  </span>
                )}
              </div>
              {item.summary && item.summary !== item.label && (
                <p className="mt-1 break-words text-caption text-foreground">{item.summary}</p>
              )}
              {item.excerpt && item.kind !== "mission_halted" && (
                <pre className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-words rounded bg-muted px-2 py-1 font-mono text-micro text-muted-foreground">
                  {item.excerpt}
                </pre>
              )}
              {item.options.length > 0 ? (
                <div className="mt-2 flex flex-wrap gap-1.5">
                  {item.options.map((choice) => (
                    <button
                      key={choice.id}
                      type="button"
                      disabled={probe.state !== "yes" || busy}
                      onClick={() => void answer(item, choice)}
                      title={choice.keys.length ? `presses ${choice.keys.join(", ")}` : undefined}
                      className={cx(
                        "inline-flex h-7 items-center gap-1 rounded-control border px-2.5 text-caption font-medium transition-colors focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-50",
                        choice.danger || choice.action === "deny"
                          ? "border-destructive/40 text-destructive hover:bg-fail-tint"
                          : "border-border-strong bg-surface-raised text-foreground hover:bg-surface-hover",
                      )}
                    >
                      {choice.label}
                    </button>
                  ))}
                </div>
              ) : (
                item.session_id && (
                  <p className="mt-1 text-caption text-faint-foreground">
                    No prompt on its pane: open the agent and type to it.
                  </p>
                )
              )}
              {probe.state === "no" && probe.reason && (
                <p className="mt-1 text-caption text-muted-foreground">Read-only here: {probe.reason}</p>
              )}
              {outcome && (
                <p
                  role="status"
                  className={cx(
                    "mt-1 flex items-center gap-1 text-caption",
                    outcome.state === "failed" ? "text-destructive" : "text-muted-foreground",
                  )}
                >
                  {outcome.state === "sending" ? (
                    <LoaderCircle size={12} className="animate-spin" />
                  ) : outcome.state === "done" ? (
                    <CircleCheck size={12} className="text-success" />
                  ) : (
                    <CircleAlert size={12} />
                  )}
                  {outcome.state === "sending" ? "Sending…" : outcome.state === "done" ? outcome.text : outcome.error}
                </p>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
