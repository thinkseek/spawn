#!/bin/sh
# Objective 120 [feature]: Home in affectUI. Home is rebuilt on the kit's
# components with the same data, wording, behaviour and routes, and its
# markup comes from one component that takes its data as props
# (components/home/HomeView.tsx: HomeView(props: HomeData)), so what a user
# sees can be rendered and read without a browser.
#
# Black box over the tree under test (the working directory):
#   (1) The UI builds (`npm run build`: tsc, then vite) into a directory of
#       the scenario's own, and every web test passes (`node --test`).
#   (2) A server-side render (react-dom/server, through `vite build --ssr`
#       of an entry the scenario writes; a bare window: an address, empty
#       storage, a fetch that fails) of HomeView with made-up data, the
#       models made by Spawn's own pure modules' types and functions:
#       - "full": two Needs-you items (one to act on with a command to copy
#         and a Resume, one to know with an Open and a judge command), a
#         mission with work in flight and a send-back, a quiet mission and 3
#         closed, an agent working and one idle, a supervisor note, both
#         intervention windows with interventions, and core lines with a
#         north star in range with 7 days of trend, a north star "measuring
#         since", a guardrail "not collected yet", and two drills ("never
#         passed", "no drill");
#       - "empty": no Needs-you item, no mission work, interventions that
#         failed to read, core lines still loading.
#       Every text the prompt keeps word for word is in the rendered text:
#       each item's title and detail, "Should not have been raised?" with
#       its command, the command to copy, the summary (needsSummary),
#       "Nothing needs you.", the missions' rows and their "Nothing open:"
#       and "<n> closed" line, noWorkText, the agents' names, the note, each
#       interventions figure and count as interventionsView words them,
#       "Could not read the interventions: <reason>", and each core line's
#       north star value, name and range, each guardrail's value and each
#       drill's text as coreLinesView words them.
#   (3) Each north star is drawn by the kit's KPI chart: an element labelled
#       "<north star name> chart" per north star.
#   (4) No rendered text or attribute says "undefined", "NaN" or
#       "[object Object]".
#   (5) Rendered under data-theme="dark" and "light": every --aui-* name
#       the rendered markup reads is declared by Spawn's styles for that
#       theme and resolves through them to a value (119's tokens).
#
# Needs node and the controller's web/node_modules (as check-web).
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
# needs: controller:web/node_modules/

set -eu

asserted=""
harness() {
    echo "120: harness: $*" >&2
    exit 2
}
controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test is the working directory: the controller's copy holds
# main's web/src, never the candidate's.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -d "$controller/web/node_modules" ] || harness "no node_modules in $controller/web"
for tool in node npm find sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
finish() {
    rc=$?
    trap - EXIT
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "120: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM
: >"$WORK/failures"
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
export TZ=UTC

cp -a "$tree/web" "$WORK/web"
rm -rf "$WORK/web/node_modules" "$WORK/web/dist"
ln -s "$controller/web/node_modules" "$WORK/web/node_modules"

# --- (1) the build and the web tests ----------------------------------------------------------
if (cd "$WORK/web" && npm run build -- --configLoader runner --outDir "$WORK/dist") >"$WORK/build.out" 2>&1; then
    echo "120: (1) checked: the UI builds" >&2
else
    check_failed "(1) the UI does not build: $(grep -E 'error|Error' "$WORK/build.out" | head -n 5 | tr '\n' ' ' | head -c 500)"
fi
if (cd "$WORK/web" && node --test) >"$WORK/test.out" 2>&1; then
    echo "120: (1) checked: $(sed -n 's/^. pass //p' "$WORK/test.out" | tail -n 1) web tests pass" >&2
else
    check_failed "(1) a web test fails: $(grep -E '^. fail|not ok' "$WORK/test.out" | head -n 3 | tr '\n' ' ')"
fi

# --- (2)-(5) the server-side render --------------------------------------------------------------
mkdir -p "$WORK/web/ssr"
cat >"$WORK/web/ssr/entry.tsx" <<'TSX'
import { writeFileSync } from "node:fs";
import { renderToString } from "react-dom/server";

import { HomeView } from "../src/components/home/HomeView";
import type { HomeData } from "../src/components/home/HomeView";
import { coreLinesView } from "../src/coreLines.ts";
import type { CoreLinesAnswer } from "../src/coreLines.ts";
import { needsSummary, noWorkText, quietText } from "../src/home";
import type { MissionsToday, NeedItem } from "../src/home";
import { figureState } from "../src/interventions.ts";
import type { InterventionsAnswer } from "../src/interventions.ts";

const out = process.argv[2];
const now = Date.UTC(2026, 10, 13, 12, 0, 0);
const H = 3_600_000;

const needs: NeedItem[] = [
  {
    key: "need-halt", tone: "act", title: "Mission #4 halted", detail: "made-up halt reason, gate refused",
    since_ms: now - 10 * 60_000, command: "spawn mission resume 4 --halt 91", resume: { mission: 4, halt: 91 },
    session: null, judge: null,
  },
  {
    key: "need-alert", tone: "know", title: "Made-up alert disk-low", detail: "made-up alert detail line",
    since_ms: now - H, command: null, session: "session_madeup", judge: "spawn alert judge disk-low false|stale|duplicate",
  },
];
const missions: MissionsToday = {
  rows: [
    {
      id: 4, name: "Made-up mission", halted: null, paused: false,
      inFlight: [{ task_id: 301, title: "made-up task title", state: "running", since_ms: now - 60_000, conflicts: 0, earlierConflicts: 0, conflictText: null }],
      landedToday: 2,
      lastRejection: { task_id: 299, verdict: "rejected", reason: "made-up rejection reason", at_ms: now - 2 * H },
    },
  ],
  quiet: [{ id: 6, name: "Quiet made-up mission", idle_ms: 3 * H }],
  closed: 3,
};
const emptyMissions: MissionsToday = { rows: [], quiet: [], closed: 0 };
const item = (at: number, kind: string, line: string) => ({
  kind, at_ms: at, principal: "owner", agent: "worker-alpha", session: "s1", line, commands: [11, 12], attempts: 1, outcome: "taken",
});
const answer = (window: string, total: number, hours: number): InterventionsAnswer => ({
  window, since_ms: now - (window === "24h" ? 24 : 168) * H, until_ms: now, agent_hours: hours, total,
  by_kind: { command: total }, retries: 1, per_agent_hour: total / hours,
  items: Array.from({ length: total }, (_, n) => item(now - (n + 1) * H, "command", `made-up intervention ${n + 1}`)),
});
const day = figureState(answer("24h", 2, 8), null);
const week = figureState(answer("7d", 5, 40), null);
const trend = Array.from({ length: 7 }, (_, n) => ({ day: `11-${String(7 + n).padStart(2, "0")}`, value: 1100 + 30 * n }));
const core: CoreLinesAnswer = {
  lines: [
    {
      line: "performance",
      north_star: { key: "perf.latency", name: "Made-up latency", unit: "ms", value: 1234, range: { max: 2000 }, state: "in", trend },
      guardrails: [{ key: "perf.guard", name: "Made-up guard", unit: "count", value: 3, range: { max: 5 }, state: "in", trend: [] }],
    },
    {
      // Objective 176: the drills show under the sustainability row.
      line: "sustainability",
      north_star: {
        key: "rec.age", name: "Made-up restore age", unit: "days", value: null, range: { max: 2 }, state: "not_collected",
        measuring_since_ms: now - 3 * 24 * H,
      },
      guardrails: [{ key: "rec.guard", name: "Uncollected made-up guard", unit: "count", value: null, range: null, state: "not_collected" }],
    },
  ],
  ranges_file: null, ranges_error: null, ignored_ranges: [],
  drills: [
    { name: "made-up drill A", source: "unit drill-a", passed_at_ms: null, last: null },
    { name: "made-up drill B", source: null, passed_at_ms: null },
  ],
};

const base = {
  now, back: null, talk: <span>made-up talk box</span>, onOpenAgent: () => {}, onShowMissions: () => {},
  digest: { since_ms: now - 24 * H, away: false, landed: 2, sentBack: 1, alerts: 1, entries: [], bands: [] },
};
const full: HomeData = {
  ...base, needs, missions,
  agents: {
    working: [{ session_id: "s1", name: "worker-alpha", task: "T301", doing: "editing a made-up file", since_ms: now - 30_000 }],
    idle: [{ session_id: "s2", name: "worker-beta", since_ms: now - 15 * 60_000 }],
  },
  notes: { shown: [{ text: "made-up supervisor note", at_ms: now - 60_000, taskStates: "T301 running" }], more: 0, earlier: 0, settled: 0 },
  allNotes: [{ text: "made-up supervisor note", at_ms: now - 60_000, settled: false, tasks: "T301 running", current: true }],
  interventions: { day, week },
  coreLines: { answer: core, error: null },
};
const empty: HomeData = {
  ...base, needs: [], missions: emptyMissions,
  agents: { working: [], idle: [] },
  notes: { shown: [], more: 0, earlier: 0, settled: 0 },
  allNotes: [],
  interventions: { day: figureState(null, "made-up failure 500"), week: figureState(null, "made-up failure 500") },
  coreLines: { answer: null, error: null },
};

const view = coreLinesView(core, now);
const words = (state: ReturnType<typeof figureState>) => (state.kind === "view" ? [state.view.figure, state.view.count] : []);
const expectFull = [
  ...needs.flatMap((need) => [need.title, need.detail ?? "", need.command ?? "", need.judge ?? ""]),
  "Should not have been raised?",
  needsSummary(needs),
  "Made-up mission", "made-up task title", `Nothing open: ${quietText(missions.quiet)}`, "3 closed",
  "worker-alpha", "worker-beta", "made-up supervisor note",
  ...words(day), ...words(week),
  ...view.rows.flatMap((row) => [row.north_star.name, row.north_star.value, row.north_star.range, ...row.guardrails.map((g) => g.value)]),
  ...view.drills.map((drill) => drill.text),
].filter((text) => text !== "");
const expectEmpty = ["Nothing needs you.", noWorkText(emptyMissions), "Could not read the interventions: made-up failure 500"];
const charts = view.rows.map((row) => `${row.north_star.name} chart`);

const render = (data: HomeData, theme: string) => renderToString(<div data-theme={theme}><HomeView {...data} /></div>);
writeFileSync(
  out,
  JSON.stringify({
    renders: [
      { name: "full", theme: "dark", html: render(full, "dark"), expect: expectFull, charts },
      { name: "full", theme: "light", html: render(full, "light"), expect: expectFull, charts },
      { name: "empty", theme: "dark", html: render(empty, "dark"), expect: expectEmpty, charts: [] },
    ],
  }),
);
TSX
if ! (cd "$WORK/web" && npx --no-install vite build --configLoader runner --ssr ssr/entry.tsx --outDir ssr-out) >"$WORK/ssr-build.out" 2>&1; then
    # The candidate has no HomeView taking its data as props, or it does not build.
    check_failed "(2) the server-side render of HomeView does not build: $(grep -E 'error|Error|not exported|Could not resolve' "$WORK/ssr-build.out" | sed 's/\x1b\[[0-9;]*m//g' | head -n 3 | tr '\n' ' ' | head -c 400)"
else
    entry=$(find "$WORK/web/ssr-out" -name 'entry.*js' | head -n 1)
    [ -n "$entry" ] || harness "the SSR build wrote no entry: $(ls "$WORK/web/ssr-out" | tr '\n' ' ')"
    # A bare window for modules that read the page's address or storage at
    # load (api.ts reads ?token=): nothing is fetched, nothing is stored.
    cat >"$WORK/window.mjs" <<'JS'
const store = () => { const m = new Map(); return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k) }; };
globalThis.window = globalThis;
globalThis.location = new URL("http://127.0.0.1/");
globalThis.localStorage = store();
globalThis.sessionStorage = store();
globalThis.fetch = () => Promise.reject(new Error("no fetch in the render"));
JS
    if ! (cd "$WORK/web" && node --import "$WORK/window.mjs" "$entry" "$WORK/renders.json") >"$WORK/ssr-run.out" 2>&1; then
        check_failed "(2) the server-side render of HomeView fails: $(grep -m 3 -E 'Error|at ' "$WORK/ssr-run.out" | tr '\n' ' ' | head -c 400)"
    else
        cat >"$WORK/check.mjs" <<'JS'
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
const [renders, web] = process.argv.slice(2);
const { renders: list } = JSON.parse(readFileSync(renders, "utf8"));
const unescape = (s) => s.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&#x27;|&#39;/g, "'").replace(/&amp;/g, "&");
// Spawn's styles, for (5): rules naming the theme, declarations in order,
// aui.css (imported after spawn.css) last.
const strip = (css) => css.replace(/\/\*[\s\S]*?\*\//g, "");
const rules = (css) => [...strip(css).matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => {
  const decls = new Map();
  for (const d of m[2].split(";")) { const i = d.indexOf(":"); if (i > 0) decls.set(d.slice(0, i).trim(), d.slice(i + 1).trim()); }
  return { selector: m[1].trim(), decls };
});
const styles = join(web, "src/styles");
const files = readdirSync(styles).filter((n) => n.endsWith(".css") && n !== "aui.css").map((n) => join(styles, n));
files.push(join(web, "src/index.css"));
if (readdirSync(styles).includes("aui.css")) files.push(join(styles, "aui.css"));
const all = files.flatMap((f) => rules(readFileSync(f, "utf8")));
const resolver = (theme) => {
  const tokens = new Map();
  for (const r of all) {
    if (!r.selector.includes(`[data-theme="${theme}"]`) && !/(^|\s)@theme\b/.test(r.selector)) continue;
    for (const [k, v] of r.decls) if (k.startsWith("--")) tokens.set(k, v);
  }
  return (name) => {
    let v = tokens.get(name);
    for (let depth = 0; v !== undefined && depth < 8; depth++) {
      const ref = /^var\((--[\w-]+)\s*(?:,[^)]*)?\)$/.exec(v);
      if (!ref) return v;
      v = tokens.get(ref[1]);
    }
    return v ?? null;
  };
};
for (const r of list) {
  const at = `(${r.name}, ${r.theme})`;
  const styleless = r.html.replace(/<style[\s\S]*?<\/style>/g, "");
  const text = unescape(styleless.replace(/<[^>]*>/g, " ")).replace(/\s+/g, " ");
  for (const want of r.expect) {
    if (!text.includes(want.replace(/\s+/g, " "))) console.log(`FAIL (2) ${at} the rendered text lacks [${want}]`);
  }
  for (const label of r.charts) {
    const re = new RegExp(`aria-label="${label.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`);
    if (!re.test(unescape(r.html))) console.log(`FAIL (3) ${at} no element labelled [${label}…]: the north star is not on the kit's KPI chart`);
  }
  const attrs = [...styleless.matchAll(/="([^"]*)"/g)].map((m) => unescape(m[1])).join(" ");
  for (const bad of ["undefined", "NaN", "[object Object]"]) {
    if (text.includes(bad) || attrs.includes(bad)) console.log(`FAIL (4) ${at} the render says [${bad}]`);
  }
  const resolve = resolver(r.theme);
  const names = new Set([...r.html.matchAll(/var\((--aui-[a-z0-9-]+)/g)].map((m) => m[1]));
  if (names.size === 0) console.log(`FAIL (5) ${at} the render reads no --aui- name: no kit component in it`);
  for (const name of [...names].sort()) {
    if (resolve(name) === null) console.log(`FAIL (5) ${at} ${name} does not resolve in Spawn's styles for the ${r.theme} theme`);
  }
  console.log(`OK ${at} ${r.expect.length} texts, ${r.charts.length} charts, ${names.size} --aui- names`);
}
JS
        node "$WORK/check.mjs" "$WORK/renders.json" "$WORK/web" >"$WORK/check.out" 2>&1 ||
            harness "the render check did not run: $(head -c 300 "$WORK/check.out")"
        grep '^FAIL ' "$WORK/check.out" | sed 's/^FAIL //' | while IFS= read -r line; do check_failed "$line"; done
        [ "$(grep -c '^OK ' "$WORK/check.out")" -eq 3 ] || harness "the render check read $(grep -c '^OK ' "$WORK/check.out") renders, not 3"
        sed -n 's/^OK /120: (2)-(5) checked: /p' "$WORK/check.out" >&2
    fi
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "120: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "120: Home renders from its data on the kit, every kept text word for word, in both themes"
exit 0
