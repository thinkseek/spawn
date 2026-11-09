// Where everything stands in the Show scene, in a 1600 × 900 view box,
// and the curves marks travel. Three columns: control on the left, compute
// in the middle, storage on the right.
import { scaleLinear } from "d3-scale";
import { curveNatural, line } from "d3-shape";

import type { NodeKey, Place, Stage, Tone } from "../../show";

export const WIDTH = 1600;
export const HEIGHT = 900;

export interface Point {
  x: number;
  y: number;
}

export type Shape = "control" | "storage" | "compute";

export interface PlaceSpec extends Point {
  label: string;
  shape: Shape;
}

/** The sources' ring of GitHub around them. */
export const SOURCES: Point = { x: 1400, y: 560 };
export const GITHUB_RADIUS = 92;

export const PLACE_SPECS: Record<Exclude<Place, "supervisor">, PlaceSpec> = {
  gateway: { x: 210, y: 480, label: "gateway", shape: "control" },
  dispatcher: { x: 210, y: 650, label: "dispatcher", shape: "control" },
  bridge: { x: 690, y: 770, label: "Bridge", shape: "control" },
  integrator: { x: 1060, y: 740, label: "integrator", shape: "control" },
  gate: { x: 1230, y: 740, label: "gate", shape: "control" },
  log: { x: 1110, y: 330, label: "event log", shape: "storage" },
  transcripts: { x: 1330, y: 250, label: "transcripts", shape: "storage" },
  workspace: { x: 1060, y: 560, label: "workspaces", shape: "storage" },
  sources: { ...SOURCES, label: "mission sources", shape: "storage" },
  // A point on the ring, up and out: where a landing leaves for GitHub.
  github: {
    x: SOURCES.x + GITHUB_RADIUS * Math.cos(-Math.PI / 5),
    y: SOURCES.y + GITHUB_RADIUS * Math.sin(-Math.PI / 5),
    label: "GitHub",
    shape: "storage",
  },
};

/** Column headings, faint, above each column. */
export const COLUMNS = [
  { x: 210, label: "control" },
  { x: 690, label: "compute" },
  { x: 1220, label: "storage" },
] as const;
export const COLUMN_Y = 215;

const SUPERVISOR: Point = { x: 210, y: 310 };
const RESIDENT_ROW = { x: 590, y: 310, step: 200 };
const WORKERS = { x: 470, y: 470, columns: 6, step: 88, rowStep: 84 };

/** Fixed spot of the n-th resident (in stage order). */
function residentPoint(index: number): Point {
  return { x: RESIDENT_ROW.x + index * RESIDENT_ROW.step, y: RESIDENT_ROW.y };
}

/** A worker's spot: its slot in a grid under the residents. */
export function workerPoint(slot: number): Point {
  const column = slot % WORKERS.columns;
  const row = Math.floor(slot / WORKERS.columns) % 3;
  // Odd rows sit half a step over, so the grid reads as a loose field.
  const shift = row % 2 === 1 ? WORKERS.step / 2 : 0;
  return { x: WORKERS.x + column * WORKERS.step + shift, y: WORKERS.y + row * WORKERS.rowStep };
}

/** The residents in stage order: the standing ones first, then any other. */
export function residentOrder(stage: Stage): string[] {
  return Object.values(stage.agents)
    .filter((agent) => agent.role === "resident")
    .map((agent) => agent.agentId)
    .sort((a, b) => residentRank(a) - residentRank(b) || a.localeCompare(b));
}

function residentRank(agentId: string): number {
  if (agentId.endsWith("_rsi")) return 0;
  if (agentId.endsWith("_supply")) return 1;
  return 2;
}

/** Where a node stands now; null for an agent not on stage. */
export function pointOf(key: NodeKey, stage: Stage): Point | null {
  if (key === "supervisor") return SUPERVISOR;
  if (!key.startsWith("agent:")) {
    const place = PLACE_SPECS[key as Exclude<Place, "supervisor">];
    return place ? { x: place.x, y: place.y } : null;
  }
  const agentId = key.slice("agent:".length);
  const agent = stage.agents[agentId];
  if (!agent) return null;
  if (agent.role === "supervisor") return SUPERVISOR;
  if (agent.role === "resident") return residentPoint(residentOrder(stage).indexOf(agentId));
  return workerPoint(agent.slot);
}

const curve = line<Point>()
  .x((point) => point.x)
  .y((point) => point.y)
  .curve(curveNatural);

/** A gentle bow through the middle of each leg, always to the same side,
 * so two marks between the same nodes in opposite directions part. */
function bowed(points: Point[]): Point[] {
  const out: Point[] = [points[0]];
  for (let i = 1; i < points.length; i += 1) {
    const a = points[i - 1];
    const b = points[i];
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    const length = Math.hypot(dx, dy) || 1;
    const bow = Math.min(60, length * 0.12);
    out.push({ x: (a.x + b.x) / 2 - (dy / length) * bow, y: (a.y + b.y) / 2 + (dx / length) * bow });
    out.push(b);
  }
  return out;
}

/** The SVG path of a route, or null when one of its nodes is gone. */
export function routePath(route: NodeKey[], stage: Stage): string | null {
  const points = route.map((key) => pointOf(key, stage));
  if (points.some((point) => point === null)) return null;
  return curve(bowed(points as Point[]));
}

/** Seconds a mark takes: longer paths a little slower, never sluggish. */
export const travelSeconds = scaleLinear().domain([0, 1400]).range([1.1, 3.2]).clamp(true);

/** Mark colours: the trace kinds' slots (spawn.css --trace-*, muted and
 * stepped per theme), one per kind of work, the attention mark for a
 * landing, and a quiet neutral for plain log appends. CSS variables, so the
 * scene follows the theme. */
export const TONE_COLOR: Record<Tone, string> = {
  commit: "var(--trace-execution)",
  command: "var(--trace-turn)",
  task: "var(--trace-tool)",
  landing: "var(--wait-mark)",
  trace: "var(--trace-command)",
};

export const TONE_LABEL: Record<Tone, string> = {
  trace: "event appended",
  commit: "commit",
  command: "command",
  task: "task / gate",
  landing: "landing",
};

/** Status colours, reserved for the gate's verdict and always labelled. */
export const VERDICT_COLOR = { accepted: "var(--ok-mark)", rejected: "var(--fail-mark)" } as const;
