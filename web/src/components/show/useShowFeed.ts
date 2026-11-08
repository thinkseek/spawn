// The Show view's feed: loads events (live tail or a replay window), runs
// the scene clock on a Motion value, and hands each event to the scene as
// the clock passes its record time. Live mode runs the clock at 1×, a few
// seconds behind the server, so polled batches play out spread as they
// happened; replay runs it over the last hours at 10–60×, and loops.
import { animate, useMotionValue, useMotionValueEvent } from "motion/react";
import type { MotionValue } from "motion/react";
import { useCallback, useEffect, useRef, useState } from "react";

import { fetchEvents, fetchEventsSince } from "../../events";
import { pageHidden } from "../../validated";
import {
  admitMarks,
  controlRecord,
  effectsOf,
  emptyStage,
  eventsPerMinute,
  gatePassRate,
  landedToday,
  landings,
  prune,
  applyEvent,
  runningCount,
  stageFromFleet,
  startOfDay,
} from "../../show";
import type { LogEvent, Mode, Stage, Tone } from "../../show";
import { pointOf, routePath, travelSeconds } from "./layout";
import type { Point } from "./layout";

export type SceneMark =
  | { id: number; kind: "travel"; tone: Tone; d: string; seconds: number; end: Point }
  | { id: number; kind: "spark"; tone: Tone; at: Point; angle: number }
  | { id: number; kind: "verdict"; tone: Tone; at: Point; verdict: string; accepted: boolean };

export interface ShowNumbers {
  running: number;
  perMinute: number;
  landed: number;
  passRate: number | null;
  /** Scene time each task last landed, by task id. */
  landings: Map<number, number>;
  /** Scene time of these numbers. */
  at: number;
}

export type FeedStatus = "loading" | "running" | "error";

export interface ShowFeed {
  stage: Stage;
  marks: SceneMark[];
  numbers: ShowNumbers;
  clock: MotionValue<number>;
  /** Replay window, scene time. */
  span: { from: number; to: number } | null;
  status: FeedStatus;
  error: string | null;
  removeMark: (id: number) => void;
}

/** How far behind the server the live clock runs: a poll's batch arrives
 * before its events are due. */
const LIVE_LAG_MS = 4_000;
const POLL_MS = 1_500;
const DAY_MS = 86_400_000;
/** The numbers redraw at most this often (wall time). */
const NUMBERS_EVERY_MS = 400;

const NO_NUMBERS: ShowNumbers = { running: 0, perMinute: 0, landed: 0, passRate: null, landings: new Map(), at: 0 };

let markIds = 0;

function isControl(event: LogEvent): boolean {
  return controlRecord(event) !== null;
}

export function useShowFeed({
  mode,
  speed,
  hours,
  fleetAgents,
}: {
  mode: Mode;
  speed: number;
  hours: number;
  fleetAgents: { agent_id: string; live: boolean; execution_id: string | null }[] | null;
}): ShowFeed {
  const clock = useMotionValue(0);
  const [stage, setStage] = useState<Stage>(emptyStage);
  const [marks, setMarks] = useState<SceneMark[]>([]);
  const [numbers, setNumbers] = useState<ShowNumbers>(NO_NUMBERS);
  const [status, setStatus] = useState<FeedStatus>("loading");
  const [error, setError] = useState<string | null>(null);
  const [span, setSpan] = useState<{ from: number; to: number } | null>(null);
  // A replay's loaded events, to start it over when it reaches the end.
  const [replay, setReplay] = useState<{ events: LogEvent[]; ledger: LogEvent[]; from: number; to: number; round: number } | null>(null);

  const stageRef = useRef<Stage>(stage);
  const queue = useRef<LogEvent[]>([]);
  const cursor = useRef(0);
  // Control records (for the day's numbers) and the last minute of events.
  const seen = useRef<LogEvent[]>([]);
  const numbersAt = useRef(0);
  const fleetRef = useRef(fleetAgents);
  fleetRef.current = fleetAgents;

  const putStage = useCallback((next: Stage) => {
    if (next === stageRef.current) return;
    stageRef.current = next;
    setStage(next);
  }, []);

  const reset = useCallback(
    (next: Stage, ledger: LogEvent[], events: LogEvent[]) => {
      queue.current = events;
      cursor.current = 0;
      seen.current = ledger;
      numbersAt.current = 0;
      putStage(next);
      setMarks([]);
    },
    [putStage],
  );

  const refreshNumbers = useCallback((now: number, force = false) => {
    const wall = performance.now();
    if (!force && wall - numbersAt.current < NUMBERS_EVERY_MS) return;
    numbersAt.current = wall;
    seen.current = seen.current.filter((event) => isControl(event) || event.recorded_at_ms > now - 60_000);
    setNumbers({
      running: runningCount(stageRef.current),
      perMinute: eventsPerMinute(seen.current, now),
      landed: landedToday(seen.current, now),
      passRate: gatePassRate(seen.current, now),
      landings: landings(seen.current, now),
      at: now,
    });
  }, []);

  // Hand the scene every event the clock has passed.
  useMotionValueEvent(clock, "change", (now) => {
    const due: LogEvent[] = [];
    const events = queue.current;
    while (cursor.current < events.length && events[cursor.current].recorded_at_ms <= now) {
      due.push(events[cursor.current]);
      cursor.current += 1;
    }
    if (mode === "live" && cursor.current > 5_000) {
      queue.current = events.slice(cursor.current);
      cursor.current = 0;
    }
    let next = stageRef.current;
    const added: SceneMark[] = [];
    for (const event of due) {
      next = applyEvent(next, event);
      for (const effect of effectsOf(event)) {
        if (effect.kind === "travel") {
          const d = routePath(effect.route, next);
          const end = pointOf(effect.route[effect.route.length - 1], next);
          if (d === null || end === null) continue;
          const length = effect.route.length * 320;
          added.push({ id: (markIds += 1), kind: "travel", tone: effect.tone, d, seconds: travelSeconds(length), end });
        } else if (effect.kind === "spark") {
          const at = pointOf(effect.node, next);
          if (at) added.push({ id: (markIds += 1), kind: "spark", tone: "trace", at, angle: (event.id * 137.5) % 360 });
        } else {
          const at = pointOf(effect.node, next);
          if (at) {
            added.push({ id: (markIds += 1), kind: "verdict", tone: "task", at, verdict: effect.verdict, accepted: effect.accepted });
          }
        }
      }
    }
    seen.current.push(...due);
    putStage(prune(next, now));
    if (added.length > 0) setMarks((current) => admitMarks(current, added));
    refreshNumbers(now, due.length > 0 && numbersAt.current === 0);
  });

  // Live: the last minute for the numbers, the day's control records in
  // the background, then a poll after the last position.
  useEffect(() => {
    if (mode !== "live") return;
    const controller = new AbortController();
    const { signal } = controller;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let running: { stop: () => void } | undefined;
    // Polls at once when the page shows again (it reads nothing hidden).
    let pollNow: (() => void) | undefined;
    const onVisibility = () => {
      if (!pageHidden(document)) pollNow?.();
    };
    document.addEventListener("visibilitychange", onVisibility);
    setStatus("loading");
    setError(null);
    setSpan(null);
    (async () => {
      try {
        const nowGuess = Date.now();
        const recent = await fetchEventsSince(nowGuess - 60_000, signal);
        const offset = recent.now_ms - Date.now();
        const start = Date.now() + offset - LIVE_LAG_MS;
        const fleet = fleetRef.current;
        let initial = fleet ? stageFromFleet(fleet) : emptyStage();
        // What the last minute already did, without marks.
        for (const event of recent.events) {
          if (event.recorded_at_ms <= start) initial = applyEvent(initial, event);
        }
        reset(
          initial,
          recent.events.filter((event) => event.recorded_at_ms <= start),
          recent.events.filter((event) => event.recorded_at_ms > start),
        );
        clock.jump(start);
        running = animate(clock, start + DAY_MS, { duration: DAY_MS / 1000, ease: "linear" });
        setStatus("running");
        let after = recent.next;
        const firstId = recent.events[0]?.id ?? after + 1;
        // The day so far, control records only, for landed today and the
        // gate pass rate.
        void fetchEventsSince(startOfDay(recent.now_ms), signal, { before: firstId, keep: isControl })
          .then((day) => {
            seen.current = [...day.events, ...seen.current];
          })
          .catch(() => undefined);
        let polling = false;
        const poll = async () => {
          if (polling) return;
          if (timer !== undefined) clearTimeout(timer);
          // A hidden page reads nothing; it reads at once when it shows.
          if (pageHidden(document)) return;
          polling = true;
          try {
            const page = await fetchEvents({ after }, signal);
            after = page.next;
            queue.current.push(...page.events);
            setError(null);
          } catch (caught) {
            if (signal.aborted) return;
            setError(caught instanceof Error ? caught.message : String(caught));
          } finally {
            polling = false;
          }
          if (!signal.aborted) timer = setTimeout(poll, POLL_MS);
        };
        pollNow = () => void poll();
        timer = setTimeout(poll, POLL_MS);
      } catch (caught) {
        if (signal.aborted) return;
        setStatus("error");
        setError(caught instanceof Error ? caught.message : String(caught));
      }
    })();
    return () => {
      controller.abort();
      if (timer !== undefined) clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      running?.stop();
    };
  }, [mode, clock, reset]);

  // Replay: load the window (and the day's control records before it).
  useEffect(() => {
    if (mode !== "replay") return;
    const controller = new AbortController();
    const { signal } = controller;
    setStatus("loading");
    setError(null);
    setReplay(null);
    (async () => {
      try {
        const now = Date.now();
        const from = now - hours * 3_600_000;
        const page = await fetchEventsSince(from, signal);
        const to = page.now_ms;
        const firstId = page.events[0]?.id ?? page.next + 1;
        const ledger = await fetchEventsSince(startOfDay(from), signal, { before: firstId, keep: isControl });
        setReplay({ events: page.events, ledger: ledger.events, from, to, round: 0 });
      } catch (caught) {
        if (signal.aborted) return;
        setStatus("error");
        setError(caught instanceof Error ? caught.message : String(caught));
      }
    })();
    return () => controller.abort();
  }, [mode, hours]);

  // Replay: run the clock from where it is to the end at `speed`; at the
  // end, start the round over.
  const replayKey = replay ? `${replay.from}:${replay.round}` : null;
  const startedKey = useRef<string | null>(null);
  useEffect(() => {
    if (mode !== "replay" || !replay) return;
    if (startedKey.current !== replayKey) {
      startedKey.current = replayKey;
      reset(emptyStage(), [...replay.ledger], replay.events);
      clock.jump(replay.from);
      setSpan({ from: replay.from, to: replay.to });
      setStatus("running");
    }
    const left = Math.max(0, replay.to - clock.get());
    const running = animate(clock, replay.to, {
      duration: left / speed / 1000,
      ease: "linear",
      onComplete: () => setReplay((current) => current && { ...current, round: current.round + 1 }),
    });
    return () => running.stop();
  }, [mode, replay, replayKey, speed, clock, reset]);

  const removeMark = useCallback((id: number) => {
    setMarks((current) => current.filter((mark) => mark.id !== id));
  }, []);

  return { stage, marks, numbers, clock, span, status, error, removeMark };
}
