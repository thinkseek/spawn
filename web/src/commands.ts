/* Commands in words (UI note §4.3): who sent what, and runs of the same.
 *
 * A command is what a principal asked of a session: an input, a resume, a
 * stop. Its row reads `Dispatcher → input · <gist>`, where the gist is what
 * the text says that the next one does not: a dispatcher digest's items,
 * not its boilerplate header. Consecutive commands from the same principal
 * with the same action fold into one run (`Dispatcher → input ×6`), which
 * opens to list each. Pure, tested in web/test/commands.test.ts. */

/** A principal by the name a reader knows it by. */
export const PRINCIPAL_LABEL: Record<string, string> = {
  human: "You",
  spawn_supervisor: "Supervisor",
  "spawn-dispatch": "Dispatcher",
  "spawn-liveness": "Liveness",
  spawn: "Spawn",
};

export function principalWords(principal: string): string {
  return PRINCIPAL_LABEL[principal] ?? principal;
}

/** The header a dispatcher digest starts with: its count and how to check
 * a worker. The same in every digest, so it says nothing about this one. */
const DIGEST_HEADER = /^Spawn dispatcher: \d+ updates? for you\b/;

/** What a command's text says, in one line: a digest's items (`- …` lines,
 * the first and "+N more"), else its first non-empty line. Null for no
 * text. */
export function commandGist(text: string | null): string | null {
  if (!text) return null;
  const lines = text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
  if (lines.length === 0) return null;
  if (DIGEST_HEADER.test(lines[0])) {
    const items = lines.slice(1).filter((line) => line.startsWith("- ")).map((line) => line.slice(2));
    if (items.length > 0) {
      return items.length === 1 ? items[0] : `${items[0]} (+${items.length - 1} more)`;
    }
  }
  return lines[0];
}

/** `Dispatcher → input`: who sent it and what it is. */
export function commandHead(principal: string, action: string): string {
  return `${principalWords(principal)} → ${action}`;
}

/** A command's row: `Dispatcher → input · <gist>`. */
export function commandWords(principal: string, action: string, text: string | null): string {
  const gist = commandGist(text);
  return gist ? `${commandHead(principal, action)} · ${gist}` : commandHead(principal, action);
}

/** A run of consecutive commands with the same principal and action, in
 * the order given. */
export interface CommandRun<T> {
  first: T;
  items: T[];
}

/** Fold consecutive commands with the same principal and action. */
export function foldCommands<T extends { principal: string; action: string }>(records: readonly T[]): CommandRun<T>[] {
  const runs: CommandRun<T>[] = [];
  for (const record of records) {
    const last = runs.at(-1);
    if (last && last.first.principal === record.principal && last.first.action === record.action) {
      last.items.push(record);
    } else {
      runs.push({ first: record, items: [record] });
    }
  }
  return runs;
}

/** How long an input given to a standing agent (a supervisor, a resident)
 * leads its page: the note's limit for the owner's notes (§4.6). A
 * worker's input is its assignment and leads however old. */
export const INPUT_FRESH_MS = 6 * 3_600_000;

/** When `task` (the board's latest person-or-supervisor input, cut to 600
 * characters) was given: the newest command whose text it is, else null
 * (older than the commands read). */
export function inputAt(
  task: string,
  records: readonly { text: string | null; created_at_ms: number | null }[],
): number | null {
  const found = records.find((record) => record.text !== null && record.text.slice(0, 600) === task);
  return found?.created_at_ms ?? null;
}

/** Does the input lead the agent's page, or fold as an older input? A
 * worker's leads; a standing agent's leads while fresh, and an input not
 * among the commands read is not fresh. */
export function inputLeads(role: string, at: number | null, now: number): boolean {
  if (role === "worker") return true;
  return at !== null && now - at <= INPUT_FRESH_MS;
}
