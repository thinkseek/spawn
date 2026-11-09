// `#/show`: a calm, full-bleed picture of Spawn at work, driven by the
// event log. The scene is SVG (Scene.tsx); over it, a few live numbers in
// large type, the missions' open objectives, a key to the marks and a
// small control for live or replay.
import { MotionConfig, motion, useMotionValueEvent, useReducedMotion, useTransform } from "motion/react";
import { useState } from "react";

import type { PollResult } from "../../api";
import { usePoll } from "../../api";
import type { Fleet } from "../../fleet";
import { clockTime } from "../../format";
import { focusObjective, fetchDashboard } from "../../rsi";
import type { RsiMission } from "../../rsi";
import { REPLAY_HOURS, REPLAY_SPEEDS } from "../../show";
import type { Mode, Tone } from "../../show";
import { FOCUS_RING, chip, cx } from "../ui";
import { TONE_COLOR, TONE_LABEL } from "./layout";
import { Scene } from "./Scene";
import { useShowFeed } from "./useShowFeed";
import type { ShowFeed, ShowNumbers } from "./useShowFeed";

export function ShowView({ fleet, present }: { fleet: PollResult<Fleet>; present: boolean }) {
  // `#/show?replay` opens in replay mode.
  const [mode, setMode] = useState<Mode>(() =>
    new URLSearchParams(window.location.hash.split("?")[1] ?? "").has("replay") ? "replay" : "live",
  );
  const [speed, setSpeed] = useState<number>(30);
  const [hours, setHours] = useState<number>(3);
  const reduced = useReducedMotion() ?? false;
  const feed = useShowFeed({ mode, speed, hours, fleetAgents: fleet.data?.agents ?? null });
  const missions = usePoll("show-missions", fetchDashboard, 15_000);

  return (
    <MotionConfig reducedMotion="user">
      <main className="relative min-h-0 min-w-0 flex-1 overflow-hidden bg-page-canvas">
        <Scene stage={feed.stage} marks={feed.marks} reduced={reduced} onMarkDone={feed.removeMark} />
        <Numbers numbers={feed.numbers} />
        <Missions missions={missions.data?.missions ?? []} numbers={feed.numbers} />
        <div className={cx("absolute right-6 bottom-5 flex flex-col items-end gap-3", present && "opacity-60 hover:opacity-100")}>
          <Key />
          <TimeControl
            feed={feed}
            mode={mode}
            speed={speed}
            hours={hours}
            onMode={setMode}
            onSpeed={setSpeed}
            onHours={setHours}
          />
        </div>
      </main>
    </MotionConfig>
  );
}

function Numbers({ numbers }: { numbers: ShowNumbers }) {
  const rate = numbers.passRate === null ? "—" : `${Math.round(numbers.passRate * 100)}%`;
  const items: [string, string][] = [
    [String(numbers.running), "agents running"],
    [String(numbers.perMinute), "events / min"],
    [String(numbers.landed), "landed today"],
    [rate, "gate pass rate"],
  ];
  return (
    <dl className="pointer-events-none absolute top-6 left-8 flex flex-wrap gap-x-14 gap-y-4">
      {items.map(([value, label]) => (
        <div key={label} className="flex flex-col-reverse">
          <dt className="mt-1 text-caption text-muted-foreground">{label}</dt>
          <dd className="text-display font-light tracking-tight text-foreground">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

/** Mission names on the wall: by resident, else the mission's own text. */
function missionName(mission: RsiMission): string {
  const resident = mission.resident?.name;
  if (resident === "rsi") return "Spawn";
  if (resident === "supply") return "Supply PA";
  return mission.text?.split("\n")[0]?.slice(0, 40) || `mission ${mission.id}`;
}

/** The objective being worked on when it is open, else the next open one. */
function openObjective(mission: RsiMission) {
  const focus = focusObjective(mission);
  if (focus?.status === "open") return focus;
  return mission.objectives.find((objective) => objective.id === mission.next_objective) ?? null;
}

/** The mission's last landing the scene has seen, by scene time: in
 * replay it agrees with the clock, not with the wall. */
function lastLanding(mission: RsiMission, numbers: ShowNumbers): number | null {
  const times = mission.tasks
    .map((task) => numbers.landings.get(task.id))
    .filter((at): at is number => at !== undefined);
  return times.length > 0 ? Math.max(...times) : null;
}

function landedText(at: number | null, now: number): string {
  if (at === null) return "no landing yet";
  const minutes = Math.max(0, Math.round((now - at) / 60_000));
  if (minutes < 1) return `landed just now · ${clockTime(at).slice(0, 5)}`;
  if (minutes < 120) return `landed ${minutes}m ago · ${clockTime(at).slice(0, 5)}`;
  return `landed ${Math.round(minutes / 60)}h ago · ${clockTime(at).slice(0, 5)}`;
}

function Missions({ missions, numbers }: { missions: RsiMission[]; numbers: ShowNumbers }) {
  if (missions.length === 0) return null;
  return (
    <div className="pointer-events-none absolute bottom-5 left-8 flex max-w-[60%] flex-wrap gap-x-12 gap-y-3">
      {missions.slice(0, 3).map((mission) => {
        const open = openObjective(mission)?.title ?? null;
        return (
          <div key={mission.id} className="min-w-0 max-w-80">
            <div className="text-body font-medium text-foreground">{missionName(mission)}</div>
            <div className="truncate text-caption text-muted-foreground">
              {open ? `open: ${open}` : mission.halted ? "halted" : "no open objective"}
            </div>
            <div className="tnum text-caption text-faint-foreground">{landedText(lastLanding(mission, numbers), numbers.at)}</div>
          </div>
        );
      })}
    </div>
  );
}

const KEY_TONES: Tone[] = ["commit", "command", "task", "landing", "trace"];

/** What each mark colour means: identity never rests on colour alone. */
function Key() {
  return (
    <ul className="flex flex-wrap justify-end gap-x-4 gap-y-1 text-micro text-faint-foreground">
      {KEY_TONES.map((tone) => (
        <li key={tone} className="flex items-center gap-1.5">
          <span className="size-2 rounded-full" style={{ background: TONE_COLOR[tone] }} />
          {TONE_LABEL[tone]}
        </li>
      ))}
    </ul>
  );
}

function Segment<T extends string | number>({
  options,
  value,
  label,
  onChange,
}: {
  options: readonly T[];
  value: T;
  label: (option: T) => string;
  onChange: (option: T) => void;
}) {
  return (
    <div className="flex gap-1.5">
      {options.map((option) => (
        <button
          key={option}
          type="button"
          aria-pressed={option === value}
          onClick={() => onChange(option)}
          className={cx(chip(option === value), FOCUS_RING)}
        >
          {label(option)}
        </button>
      ))}
    </div>
  );
}

function TimeControl({
  feed,
  mode,
  speed,
  hours,
  onMode,
  onSpeed,
  onHours,
}: {
  feed: ShowFeed;
  mode: Mode;
  speed: number;
  hours: number;
  onMode: (mode: Mode) => void;
  onSpeed: (speed: number) => void;
  onHours: (hours: number) => void;
}) {
  const [now, setNow] = useState<number | null>(null);
  useMotionValueEvent(feed.clock, "change", (value) => {
    const second = Math.floor(value / 1000) * 1000;
    if (second !== now) setNow(second);
  });
  const span = feed.span;
  const progress = useTransform(feed.clock, (value) =>
    span && span.to > span.from ? Math.min(1, Math.max(0, (value - span.from) / (span.to - span.from))) : 0,
  );
  const status =
    feed.status === "loading"
      ? mode === "replay"
        ? `loading the last ${hours}h…`
        : "connecting…"
      : feed.status === "error" || feed.error
        ? "event log unreachable"
        : null;
  return (
    <div className="flex flex-col items-end gap-1.5">
      <div className="flex items-center gap-3">
        <span className="tnum text-micro text-faint-foreground">
          {status ?? (mode === "live" ? `live · ${clockTime(now)}` : `replay · ${clockTime(now)}`)}
        </span>
        <Segment<Mode> options={["live", "replay"]} value={mode} label={(m) => m} onChange={onMode} />
        {mode === "replay" && (
          <>
            <Segment options={REPLAY_HOURS} value={hours as (typeof REPLAY_HOURS)[number]} label={(h) => `${h}h`} onChange={onHours} />
            <Segment options={REPLAY_SPEEDS} value={speed as (typeof REPLAY_SPEEDS)[number]} label={(s) => `${s}×`} onChange={onSpeed} />
          </>
        )}
      </div>
      {mode === "replay" && (
        <div className="h-px w-56 bg-foreground/[0.06]">
          <motion.div className="h-px origin-left bg-muted-foreground/60" style={{ scaleX: progress }} />
        </div>
      )}
    </div>
  );
}
