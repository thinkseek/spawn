// The page reads and writes only the canonical API paths (docs/api.md,
// issue 98). The server still answers the legacy spellings as aliases, so a
// page that used one would work and nobody would notice: this test does.
// The modules that fetch import "./api" without an extension, which node
// cannot load, so the paths are read from the source.
import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const SRC = fileURLToPath(new URL("../src", import.meta.url));

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(ts|tsx)$/.test(name) ? [path] : [];
  });
}

const read = (relative: string) => readFileSync(join(SRC, relative), "utf8");

/** The legacy spellings of docs/api.md's alias table. */
const LEGACY: [RegExp, string][] = [
  [/\/api\/session\//, "/api/session/<id>"],
  [/\/api\/fleet\/(?!["`'])/, "/api/fleet/<id>/commands|judgements"],
  [/\/api\/pane\//, "/api/pane/<id>"],
  [/\/api\/mission\//, "/api/mission/<id>/resume"],
  [/\/api\/missions\/task\//, "/api/missions/task/<n>"],
  [/\/api\/rsi(?![\w-])/, "/api/rsi"],
];

test("no page source names a legacy API path", () => {
  const files = sources(SRC);
  assert.ok(files.length > 20, "the sources are found");
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    for (const [pattern, alias] of LEGACY) {
      assert.doesNotMatch(text, pattern, `${file} names the legacy ${alias}`);
    }
  }
});

test("each renamed read and write is fetched at its canonical path", () => {
  const expected: [string, string][] = [
    ["api.ts", "`/api/sessions/${encodeURIComponent(id)}?after=0&limit=300`"],
    ["api.ts", "`/api/sessions/${encodeURIComponent(sessionId)}/${path}`"],
    ["api.ts", "`/api/missions/${encodeURIComponent(String(mission))}/resume`"],
    ["fleet.ts", "`/api/sessions/${sessionPath(sessionId)}/judgements?limit=${limit}`"],
    ["fleet.ts", "`/api/sessions/${sessionPath(sessionId)}/commands?limit=${limit}`"],
    ["fleet.ts", "`/api/sessions/${sessionPath(sessionId)}/pane`"],
    ["missions.ts", "`/api/tasks/${task}`"],
    ["rsi.ts", "`/api/tasks/${task}/gate`"],
    ["rsi.ts", 'readApi("/api/objectives",'],
    ["rsi.ts", "`/api/objectives/pane${query}`"],
    ["rsi.ts", "`/api/objectives/${mission}/${objective}`"],
  ];
  for (const [file, path] of expected) {
    assert.ok(read(file).includes(path), `${file} fetches ${path}`);
  }
});

test("live terminal screen capture UI names Herdr rather than tmux", () => {
  const agentDetail = read("components/fleet/AgentDetail.tsx");
  assert.ok(agentDetail.includes("`herdr ${data.terminal}`"), "live tab tooltip names herdr");
  assert.ok(agentDetail.includes("herdr capture, not stored"), "live status label names herdr capture");
  for (const file of sources(SRC)) {
    const text = readFileSync(file, "utf8");
    assert.doesNotMatch(text, /\btmux\b/i, `${file} must not reintroduce tmux UI copy or references`);
  }
});
