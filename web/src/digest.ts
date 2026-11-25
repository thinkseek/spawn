// The fleet digest (issue #91 P1b): `GET /api/context/digest`, the fleet in
// one small answer (crates/spawn-cli/src/serve/digest.rs): its states,
// nodes with power, open missions, pending approvals, the supervisor's
// notes for the owner, and a Markdown `digest_text` under 4000 bytes, shown
// in a compact drawer.
//
// The Markdown is shown as text, never as HTML: [`digestBlocks`] reads the
// few shapes the server writes (`#` headings, `- ` bullets, lines) and the
// page renders each as a React text node.
//
// Pure (no imports), tested in web/test/digest.test.ts.

export interface DigestNode {
  name: string;
  state: string;
  load1: number | null;
  battery_pct: number | null;
  on_ac: boolean | null;
  low_battery_drain: boolean;
  schedulable: boolean | null;
}

export interface DigestMission {
  id: number;
  title: string;
  state: string;
  halted: string | null;
  open_tasks: number;
  last_gate_failure: { task: number | null; verdict: string | null; reason: string | null } | null;
}

export interface Digest {
  generated_at_ms: number | null;
  fleet: { sessions: number; live: number; states: [string, number][] };
  nodes: DigestNode[];
  missions: DigestMission[];
  approvals: { id: string; kind: string; session_id: string | null; label: string }[];
  for_owner: { text: string; at_ms: number | null }[];
  digest_text: string;
  warnings: string[];
}

type JsonObject = Record<string, unknown>;

const object = (value: unknown): JsonObject | null =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as JsonObject) : null;
const list = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const text = (value: unknown): string | null => (typeof value === "string" ? value : null);
const number = (value: unknown): number | null =>
  typeof value === "number" && Number.isFinite(value) ? value : null;
const flag = (value: unknown): boolean | null => (typeof value === "boolean" ? value : null);

/** `/api/context/digest`'s answer; a part missing reads as empty. An
 * answer that is no object, or has no `digest_text`, throws. */
export function normalizeDigest(raw: unknown): Digest {
  const value = object(raw);
  if (!value || typeof value.digest_text !== "string") throw new Error("not a digest");
  const fleet = object(value.fleet);
  const states = Object.entries(object(fleet?.states) ?? {})
    .filter((entry): entry is [string, number] => number(entry[1]) !== null)
    .sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]));
  return {
    generated_at_ms: number(value.generated_at_ms),
    fleet: { sessions: number(fleet?.sessions) ?? 0, live: number(fleet?.live) ?? 0, states },
    nodes: list(value.nodes)
      .map(object)
      .filter((node): node is JsonObject => !!text(node?.name))
      .map((node) => {
        const power = object(node.power);
        return {
          name: node.name as string,
          state: text(node.state) ?? "unknown",
          load1: number(node.load1),
          battery_pct: number(power?.battery_pct),
          on_ac: power ? flag(power.on_ac) : null,
          low_battery_drain: node.low_battery_drain === true,
          schedulable: flag(node.schedulable),
        };
      }),
    missions: list(value.missions)
      .map(object)
      .filter((mission): mission is JsonObject => number(mission?.id) !== null)
      .map((mission) => {
        const failure = object(mission.last_gate_failure);
        return {
          id: mission.id as number,
          title: text(mission.title) ?? "",
          state: text(mission.state) ?? "",
          halted: text(mission.halted),
          open_tasks: number(mission.open_tasks) ?? 0,
          last_gate_failure: failure
            ? { task: number(failure.task), verdict: text(failure.verdict), reason: text(failure.reason) }
            : null,
        };
      }),
    approvals: list(value.approvals)
      .map(object)
      .filter((item): item is JsonObject => !!text(item?.id))
      .map((item) => ({
        id: item.id as string,
        kind: text(item.kind) ?? "",
        session_id: text(item.session_id),
        label: text(item.label) ?? "",
      })),
    for_owner: list(value.for_owner)
      .map(object)
      .filter((note): note is JsonObject => !!text(note?.text))
      .map((note) => ({ text: note.text as string, at_ms: number(note.at_ms) })),
    digest_text: value.digest_text,
    warnings: list(value.warnings).filter((warning): warning is string => typeof warning === "string"),
  };
}

export type DigestBlock =
  | { kind: "heading"; level: number; text: string }
  | { kind: "bullets"; items: string[] }
  | { kind: "text"; text: string };

/** The digest's Markdown as blocks: headings, runs of bullets, and other
 * lines; blank lines dropped. Inline marks (`**`, backticks) are taken
 * off; nothing is ever treated as HTML. */
export function digestBlocks(markdown: string): DigestBlock[] {
  const blocks: DigestBlock[] = [];
  const plain = (line: string) => line.replace(/\*\*(.+?)\*\*/g, "$1").replace(/`([^`]*)`/g, "$1").trim();
  for (const raw of markdown.split(/\r?\n/)) {
    const line = raw.trimEnd();
    if (line.trim() === "") continue;
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    const bullet = /^\s*[-*]\s+(.*)$/.exec(line);
    if (heading) {
      blocks.push({ kind: "heading", level: heading[1].length, text: plain(heading[2]) });
    } else if (bullet) {
      const last = blocks[blocks.length - 1];
      if (last?.kind === "bullets") last.items.push(plain(bullet[1]));
      else blocks.push({ kind: "bullets", items: [plain(bullet[1])] });
    } else {
      blocks.push({ kind: "text", text: plain(line) });
    }
  }
  return blocks;
}

/** The digest's one-line summary: "12 sessions, 4 live · 3 to answer". */
export function digestSummary(digest: Digest): string {
  const parts = [`${digest.fleet.sessions} sessions, ${digest.fleet.live} live`];
  if (digest.approvals.length > 0) parts.push(`${digest.approvals.length} to answer`);
  const halted = digest.missions.filter((mission) => mission.halted !== null).length;
  if (halted > 0) parts.push(`${halted} halted`);
  return parts.join(" · ");
}
