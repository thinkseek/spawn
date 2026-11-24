// The nodes Spawn may use (multi-host P1, `GET /api/nodes`): one compact
// row per node on the Fleet page. The server says each node's `state`; a
// value it could not read stays null, nothing here guesses one.
//
// Pure (no imports), tested in web/test/nodes.test.ts.

export type NodeState = "live" | "lost" | "drained";

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
}

type JsonObject = Record<string, unknown>;

const object = (value: unknown): JsonObject | null =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as JsonObject) : null;

const number = (value: unknown): number | null =>
  typeof value === "number" && Number.isFinite(value) ? value : null;

const text = (value: unknown): string | null => (typeof value === "string" && value !== "" ? value : null);

const STATES: readonly string[] = ["live", "lost", "drained"];

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
      };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
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
