// The nodes Spawn may use (multi-host P1, `GET /api/nodes`): one compact
// row per node on the Fleet page. The server says each node's `state`; a
// value it could not read stays null, nothing here guesses one.
//
// Pure (no imports), tested in web/test/nodes.test.ts.

export type NodeState = "live" | "lost" | "drained";
/** How the primary reaches a node (issue #85): ssh, or the node's own
 * `spawn node serve` over tcp or udp. */
export type NodeTransport = "ssh" | "tcp" | "udp";
/** How that path was made: straight, through relays, or a hole the node punched. */
export type NodeMode = "direct" | "relay" | "hole_punch";

export interface NodeRow {
  name: string;
  state: NodeState;
  arch: string | null;
  cores: number | null;
  /** The cgroup's CPU limit in cores (`cpu.max` quota / period); null when none. */
  cpu_limit: number | null;
  /** MemTotal, or the cgroup's memory.max when it is lower. */
  mem_bytes: number | null;
  /** Free disk of the node user's $HOME: the latest heartbeat's, else the join's. */
  disk_free_bytes: number | null;
  /** The first GPU's name and memory, e.g. "RTX 4070 Ti SUPER 16G"; null without one. */
  gpu: string | null;
  /** When the latest heartbeat was sampled (every ten minutes while it answers). */
  sampled_ms: number | null;
  /** When it was recorded lost; null unless it is. */
  lost_since_ms: number | null;
  /** ssh for a join that says none (every join before issue #85). */
  transport: NodeTransport;
  /** relay with hops, direct without, when the join says none. */
  mode: NodeMode;
  /** The hops from the primary to the node, in order; [] for none. */
  via: string[];
  /** Its power (issue #91): null for a node with no battery and no AC
   * sensor (a desktop, a VM), or one too old to say. */
  power: NodePower | null;
  /** On battery at or under the low-battery threshold: soft-drained until
   * plugged in, its `state` and the owner's drain untouched. */
  low_battery_drain: boolean;
  /** Drained by the owner, or soft-drained. */
  effective_drained: boolean;
  /** Whether work may be placed on it; null from a server that does not say. */
  schedulable: boolean | null;
}

/** A node's power, as its probes read it (crate nodes/power.rs). */
export interface NodePower {
  on_ac: boolean;
  battery_pct: number | null;
  charging: boolean;
  remaining_mins: number | null;
}

type JsonObject = Record<string, unknown>;

const object = (value: unknown): JsonObject | null =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as JsonObject) : null;

const number = (value: unknown): number | null =>
  typeof value === "number" && Number.isFinite(value) ? value : null;

const text = (value: unknown): string | null => (typeof value === "string" && value !== "" ? value : null);

const STATES: readonly string[] = ["live", "lost", "drained"];
const TRANSPORTS: readonly string[] = ["ssh", "tcp", "udp"];
const MODES: readonly string[] = ["direct", "relay", "hole_punch"];

/** A node's hops: the names of a list, anything else dropped. */
function hops(via: unknown): string[] {
  return Array.isArray(via) ? via.filter((hop): hop is string => text(hop) !== null) : [];
}

/** `cpu.max` (`<quota> <period>` or `max <period>`) as cores; null for no limit. */
export function cpuLimit(cpuMax: unknown): number | null {
  const words = typeof cpuMax === "string" ? cpuMax.trim().split(/\s+/) : [];
  const quota = Number(words[0]);
  const period = Number(words[1]);
  if (words.length !== 2 || !(quota > 0) || !(period > 0)) return null;
  return Math.round((quota / period) * 10) / 10;
}

function gpuText(gpu: unknown): string | null {
  const first = Array.isArray(gpu) ? object(gpu[0]) : null;
  const name = text(first?.name)?.replace(/^NVIDIA\s+(GeForce\s+)?/, "");
  if (!name) return null;
  const mb = number(first?.memory_mb);
  return mb === null ? name : `${name} ${Math.round(mb / 1024)}G`;
}

/** `/api/nodes`' answer as rows, by name. One without a name or a known
 * state is left out; an answer that is not `{nodes: [...]}` throws, so the
 * page says the read failed rather than showing no node. */
export function normalizeNodes(raw: unknown): NodeRow[] {
  const nodes = object(raw)?.nodes;
  if (!Array.isArray(nodes)) throw new Error("not a list of nodes");
  return nodes
    .map(object)
    .filter((node): node is JsonObject => text(node?.name) !== null && STATES.includes(node?.state as string))
    .map((node) => {
      const beat = object(node.heartbeat);
      const total = number(node.mem_total);
      const max = number(node.mem_max);
      return {
        name: node.name as string,
        state: node.state as NodeState,
        arch: text(node.arch),
        cores: number(node.cores),
        cpu_limit: cpuLimit(node.cpu_max),
        mem_bytes: total !== null && max !== null ? Math.min(total, max) : total,
        disk_free_bytes: number(beat?.disk_free) ?? number(node.disk_free),
        gpu: gpuText(node.gpu),
        sampled_ms: number(beat?.at_ms),
        lost_since_ms: number(node.lost_since_ms),
        ...reach(node),
        power: powerOf(node.power),
        low_battery_drain: node.low_battery_drain === true,
        effective_drained: node.effective_drained === true || node.state === "drained",
        schedulable: typeof node.schedulable === "boolean" ? node.schedulable : null,
      };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
}

/** A node's `power`, or null: not an object, or no `on_ac` said. */
export function powerOf(raw: unknown): NodePower | null {
  const power = object(raw);
  if (!power || typeof power.on_ac !== "boolean") return null;
  const pct = number(power.battery_pct);
  const mins = number(power.remaining_mins);
  return {
    on_ac: power.on_ac,
    battery_pct: pct === null ? null : Math.max(0, Math.min(100, Math.round(pct))),
    charging: power.charging === true,
    remaining_mins: mins === null || mins < 0 ? null : Math.round(mins),
  };
}

/** The power as `spawn node list` says it: "battery 57% discharging
 * 3h12m", "AC 100% charging", "AC"; null without a power. */
export function powerText(row: Pick<NodeRow, "power">): string | null {
  const power = row.power;
  if (!power) return null;
  const words = [power.on_ac ? "AC" : "battery"];
  if (power.battery_pct !== null) words.push(`${power.battery_pct}%`);
  if (power.charging) words.push("charging");
  else if (!power.on_ac) words.push("discharging");
  if (power.remaining_mins !== null) {
    words.push(`${Math.floor(power.remaining_mins / 60)}h${String(power.remaining_mins % 60).padStart(2, "0")}m`);
  }
  return words.join(" ");
}

/** How the power reads at a glance: "ok" on AC or charged, "low" on
 * battery in the soft drain, "draining" on battery otherwise. */
export function powerTone(row: Pick<NodeRow, "power" | "low_battery_drain">): "ok" | "draining" | "low" | null {
  if (!row.power) return null;
  if (row.low_battery_drain) return "low";
  return row.power.on_ac || row.power.charging ? "ok" : "draining";
}

/** How a node is reached, from its join; an old join's (or an unknown
 * value's) as ssh, direct, without hops, and a mode left out read from the hops. */
function reach(node: JsonObject): Pick<NodeRow, "transport" | "mode" | "via"> {
  const transport = TRANSPORTS.includes(node.transport as string) ? (node.transport as NodeTransport) : "ssh";
  const via = hops(node.via);
  const mode = MODES.includes(node.mode as string) ? (node.mode as NodeMode) : via.length ? "relay" : "direct";
  return { transport, mode, via };
}

/** The transport badge, as `spawn node list` says it: "ssh" for a direct ssh
 * node, else "tcp/relay", "udp/hole_punch". */
export function reachText(row: Pick<NodeRow, "transport" | "mode">): string {
  if (row.transport === "ssh" && row.mode === "direct") return "ssh";
  return `${row.transport}/${row.mode}`;
}

/** The hops as a row says them, "via relay-hk → edge-gw"; null without. */
export function viaText(row: Pick<NodeRow, "via">): string | null {
  return row.via.length ? `via ${row.via.join(" → ")}` : null;
}

/** Bytes as GiB, one decimal under 10 G: "12G", "7.5G"; "—" unread. */
export function gibText(bytes: number | null): string {
  if (bytes === null) return "—";
  const gib = bytes / 1024 ** 3;
  return gib < 10 ? `${gib.toFixed(1)}G` : `${Math.round(gib)}G`;
}

/** The cores as a row says them: "16", or "16 (10 allowed)" under a cgroup limit. */
export function coresText(row: Pick<NodeRow, "cores" | "cpu_limit">): string {
  if (row.cores === null) return "—";
  if (row.cpu_limit !== null && row.cpu_limit < row.cores) return `${row.cores} (${row.cpu_limit} allowed)`;
  return String(row.cores);
}
