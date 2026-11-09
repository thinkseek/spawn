// The Show scene: places, agents and the marks travelling between them,
// all SVG in a fixed view box. Motion animates every node and mark; React
// only adds and removes them.
import { AnimatePresence, animate, motion, useMotionValue, useTransform } from "motion/react";
import { memo, useEffect, useMemo } from "react";

import type { Stage, StageAgent } from "../../show";
import {
  COLUMNS,
  COLUMN_Y,
  GITHUB_RADIUS,
  HEIGHT,
  PLACE_SPECS,
  SOURCES,
  TONE_COLOR,
  VERDICT_COLOR,
  WIDTH,
  pointOf,
  routePath,
} from "./layout";
import type { PlaceSpec } from "./layout";
import type { SceneMark } from "./useShowFeed";

const INK = "var(--ink)";
const MUTED = "var(--ink-2)";
const FAINT = "var(--ink-3)";
const GUIDE = "color-mix(in oklch, var(--ink-2) 14%, transparent)";

/** The fixed links, drawn faint so the topology reads before anything
 * moves. */
const GUIDES = [
  ["supervisor", "gateway"],
  ["gateway", "dispatcher"],
  ["dispatcher", "bridge"],
  ["integrator", "gate"],
  ["workspace", "integrator"],
  ["workspace", "sources"],
  ["log", "transcripts"],
] as const;

export function Scene({
  stage,
  marks,
  reduced,
  onMarkDone,
}: {
  stage: Stage;
  marks: SceneMark[];
  reduced: boolean;
  onMarkDone: (id: number) => void;
}) {
  const agents = Object.values(stage.agents);
  return (
    <svg
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      preserveAspectRatio="xMidYMid meet"
      className="absolute inset-0 size-full"
      role="img"
      aria-label="Spawn's compute, storage and control, with each logged event moving between them"
    >
      <g aria-hidden="true">
        {COLUMNS.map((column) => (
          <text
            key={column.label}
            x={column.x}
            y={COLUMN_Y}
            textAnchor="middle"
            fill={FAINT}
            fontSize={12}
            letterSpacing="0.14em"
            style={{ textTransform: "uppercase" }}
          >
            {column.label}
          </text>
        ))}
        <Guides stage={stage} />
        <GithubRing />
        {Object.entries(PLACE_SPECS).map(([key, place]) =>
          key === "github" ? null : <PlaceNode key={key} place={place} />,
        )}
        <AnimatePresence>
          {agents.map((agent) => (
            <AgentNode key={agent.agentId} agent={agent} stage={stage} reduced={reduced} />
          ))}
        </AnimatePresence>
        {marks.map((mark) => (
          <Mark key={mark.id} mark={mark} reduced={reduced} onDone={onMarkDone} />
        ))}
      </g>
    </svg>
  );
}

function Guides({ stage }: { stage: Stage }) {
  return (
    <g fill="none" stroke={GUIDE} strokeWidth={1}>
      {GUIDES.map(([from, to]) => {
        const d = routePath([from, to], stage);
        return d ? <path key={`${from}-${to}`} d={d} /> : null;
      })}
    </g>
  );
}

function GithubRing() {
  return (
    <g>
      <circle
        cx={SOURCES.x}
        cy={SOURCES.y}
        r={GITHUB_RADIUS}
        fill="none"
        stroke="color-mix(in oklch, var(--ink-3) 16%, transparent)"
        strokeDasharray="2 6"
      />
      <text x={SOURCES.x} y={SOURCES.y - GITHUB_RADIUS - 10} textAnchor="middle" fill={FAINT} fontSize={12}>
        GitHub
      </text>
    </g>
  );
}

/** Control places are rings, storage places small filled squares (the
 * event log larger): shape, not colour, says which is which. */
function PlaceNode({ place }: { place: PlaceSpec }) {
  const big = place.label === "event log";
  return (
    <g>
      {place.shape === "control" ? (
        <circle cx={place.x} cy={place.y} r={7} fill="none" stroke={MUTED} strokeWidth={1.5} />
      ) : (
        <rect
          x={place.x - (big ? 13 : 7)}
          y={place.y - (big ? 13 : 7)}
          width={big ? 26 : 14}
          height={big ? 26 : 14}
          rx={big ? 5 : 3}
          fill={big ? "var(--selected)" : "var(--hover)"}
          stroke={big ? MUTED : "none"}
          strokeOpacity={0.35}
        />
      )}
      <text x={place.x} y={place.y + (big ? 34 : 26)} textAnchor="middle" fill={MUTED} fontSize={13}>
        {place.label}
      </text>
    </g>
  );
}

function AgentNode({ agent, stage, reduced }: { agent: StageAgent; stage: Stage; reduced: boolean }) {
  const at = pointOf(agent.key, stage);
  if (!at) return null;
  const radius = agent.role === "worker" ? 8 : 13;
  const breathing = agent.running && !reduced;
  return (
    <motion.g
      initial={{ opacity: 0 }}
      animate={{ opacity: agent.running || agent.role !== "worker" ? 1 : 0.4 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.9, ease: "easeOut" }}
    >
      {/* The halo breathes while an execution runs. */}
      <motion.circle
        cx={at.x}
        cy={at.y}
        fill={INK}
        initial={false}
        animate={
          breathing
            ? { r: [radius + 4, radius + 11, radius + 4], opacity: [0.05, 0.13, 0.05] }
            : { r: radius + 4, opacity: agent.running ? 0.08 : 0 }
        }
        transition={breathing ? { duration: 3.6, repeat: Infinity, ease: "easeInOut" } : { duration: 0.8 }}
      />
      <motion.circle
        cx={at.x}
        cy={at.y}
        r={radius}
        style={{
          fill: agent.running ? "var(--ink-2)" : "var(--idle-mark)",
          transition: "fill 0.8s",
        }}
      />
      <text
        x={at.x}
        y={at.y + radius + 18}
        textAnchor="middle"
        fill={agent.running ? INK : FAINT}
        fontSize={agent.role === "worker" ? 11 : 13}
      >
        {agent.label}
      </text>
    </motion.g>
  );
}

const Mark = memo(function Mark({
  mark,
  reduced,
  onDone,
}: {
  mark: SceneMark;
  reduced: boolean;
  onDone: (id: number) => void;
}) {
  if (mark.kind === "travel") return <Travel mark={mark} reduced={reduced} onDone={onDone} />;
  if (mark.kind === "spark") return <Spark mark={mark} reduced={reduced} onDone={onDone} />;
  return <Verdict mark={mark} reduced={reduced} onDone={onDone} />;
});

/** A detached path, for its length and points; never drawn. */
function measure(d: string): { length: number; at: (t: number) => DOMPoint } {
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", d);
  const length = path.getTotalLength();
  return { length, at: (t) => path.getPointAtLength(t * length) };
}

function Travel({
  mark,
  reduced,
  onDone,
}: {
  mark: Extract<SceneMark, { kind: "travel" }>;
  reduced: boolean;
  onDone: (id: number) => void;
}) {
  const progress = useMotionValue(0);
  const path = useMemo(() => measure(mark.d), [mark.d]);
  const cx = useTransform(progress, (t) => (reduced ? mark.end.x : path.at(t).x));
  const cy = useTransform(progress, (t) => (reduced ? mark.end.y : path.at(t).y));
  const trace = mark.tone === "trace";
  const peak = trace ? 0.45 : 0.9;
  const opacity = useTransform(progress, [0, 0.12, 0.8, 1], [0, peak, peak, 0]);
  useEffect(() => {
    const controls = animate(progress, 1, {
      duration: reduced ? 1.2 : mark.seconds,
      ease: [0.4, 0, 0.2, 1],
      onComplete: () => onDone(mark.id),
    });
    return () => controls.stop();
  }, [progress, mark, reduced, onDone]);
  return <motion.circle r={trace ? 2 : 4} fill={TONE_COLOR[mark.tone]} cx={cx} cy={cy} style={{ opacity }} />;
}

/** A tool call: a small spark that drifts off the node and fades. */
function Spark({
  mark,
  reduced,
  onDone,
}: {
  mark: Extract<SceneMark, { kind: "spark" }>;
  reduced: boolean;
  onDone: (id: number) => void;
}) {
  const radians = (mark.angle * Math.PI) / 180;
  const reach = reduced ? 12 : 26;
  return (
    <motion.circle
      r={1.6}
      fill={INK}
      initial={{ cx: mark.at.x + Math.cos(radians) * 12, cy: mark.at.y + Math.sin(radians) * 12, opacity: 0.7 }}
      animate={{
        cx: mark.at.x + Math.cos(radians) * reach,
        cy: mark.at.y + Math.sin(radians) * reach,
        opacity: 0,
      }}
      transition={{ duration: 0.9, ease: "easeOut" }}
      onAnimationComplete={() => onDone(mark.id)}
    />
  );
}

/** The gate's verdict: one slow ring in the status colour, with the word. */
function Verdict({
  mark,
  reduced,
  onDone,
}: {
  mark: Extract<SceneMark, { kind: "verdict" }>;
  reduced: boolean;
  onDone: (id: number) => void;
}) {
  const color = mark.accepted ? VERDICT_COLOR.accepted : VERDICT_COLOR.rejected;
  return (
    <g>
      <motion.circle
        cx={mark.at.x}
        cy={mark.at.y}
        fill="none"
        stroke={color}
        strokeWidth={2}
        initial={{ r: 8, opacity: 0.8 }}
        animate={{ r: reduced ? 8 : 34, opacity: 0 }}
        transition={{ duration: 2.2, ease: "easeOut", delay: 0.6 }}
      />
      <motion.text
        x={mark.at.x}
        y={mark.at.y - 20}
        textAnchor="middle"
        fontSize={13}
        fill={INK}
        initial={{ opacity: 0 }}
        animate={{ opacity: [0, 1, 1, 0] }}
        transition={{ duration: 3.2, times: [0, 0.2, 0.75, 1], delay: 0.6 }}
        onAnimationComplete={() => onDone(mark.id)}
      >
        {mark.verdict}
      </motion.text>
    </g>
  );
}
