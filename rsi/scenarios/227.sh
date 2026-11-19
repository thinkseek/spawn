#!/bin/sh
# Objective 227 [observability]: on a narrow column the Fleet page's storage
# diagnostics keep each name whole. 225 puts which fall "Free disk lost a
# day" is at the end of its name ("(the last day's fall)", "(the week's
# fit)"), and the name was drawn truncated, so a phone's one column cut
# exactly those words to "…". A name now wraps; its value stays whole.
#
# Black box over the tree under test (the working directory), as 120.sh:
#   (1) The UI builds (`vite build`) into a directory of the scenario's own;
#       its CSS is what a browser gets.
#   (2) A server-side render (react-dom/server, through `vite build --ssr`
#       of an entry the scenario writes) of the Fleet page's StorageCard, the
#       diagnostics made by coreLinesView from a made-up GET /api/core-lines
#       answer, as FleetView does: three answers, disk_fall's basis "day",
#       "fit", and none. Each diagnostic's whole name (225's words included)
#       is the text of one element of the "Storage diagnostics" list, and
#       its value the text of another in the same row.
#   (3) No rule of the built CSS that applies at some screen width cuts a
#       name. A rule is read when its class is unprefixed or every variant is
#       a width (sm: ... 2xl:, max-sm: ...) or dark:. A width variant counts:
#       from sm up the card shares the row (the 1.25fr column), about 300 px
#       at 640 px wide, narrower than on a phone. State variants (hover:,
#       focus:, ...) and print: are not read. None sets overflow hidden or clip, text-overflow
#       ellipsis, or white-space nowrap on the element holding the name; and
#       none of the elements around it (the row, the list, the card) sets
#       white-space nowrap (it is inherited, unless the name's own rules set
#       it back), text-overflow ellipsis or a line clamp.
#       Nothing hides the name or value at some width (rev-j's R3 F1): no
#       display none or visibility hidden/collapse on them, the row, the list
#       or the card. And the name wraps (rev-k's R1 F1): the white-space it gets, its own
#       or the nearest of the row, list and card that sets it, is normal,
#       pre-wrap, pre-line or break-spaces, and no text-wrap says nowrap.
#   (4) The value is never cut either: no such rule clips its element
#       (overflow hidden or clip, text-overflow ellipsis), and it does not
#       shrink (flex-shrink 0), so the number stays whole beside the name.
#       It stays on the name's first line (rev-k's R1 F2, R3 F3): the row is a
#       flex row (flex-direction row or row-reverse) that does not wrap, aligned at the baseline or the start, and
#       the value sets no align-self that moves it.
#   (5) Every other row of the list says the same name and value as before:
#       the names are coreLinesView's, word for word (227 changes the
#       layout, never the words).
# A browser's layout is not run: the gate has none. The CSS read is the
# built file the page loads, the elements the rendered markup's.
#
# Needs node and the controller's web/node_modules (as check-web).
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
# needs: controller:web/node_modules/

set -eu

asserted=""
harness() {
    echo "227: harness: $*" >&2
    exit 2
}
controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test is the working directory: the controller's copy holds
# main's web/src, never the candidate's.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -d "$controller/web/node_modules" ] || harness "no node_modules in $controller/web"
for tool in node npx find sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
finish() {
    rc=$?
    trap - EXIT
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "227: harness: unexpected exit $rc" >&2
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

# --- (1) the build ------------------------------------------------------------------------------
(cd "$WORK/web" && npx --no-install vite build --configLoader runner --outDir "$WORK/dist") >"$WORK/build.out" 2>&1 ||
    harness "the UI does not build: $(grep -E 'error|Error' "$WORK/build.out" | head -n 5 | tr '\n' ' ' | head -c 500)"
find "$WORK/dist" -name '*.css' >"$WORK/css.list"
[ -s "$WORK/css.list" ] || harness "the build wrote no CSS"
echo "227: (1) checked: the UI builds, $(wc -l <"$WORK/css.list" | tr -d ' ') CSS file(s)" >&2

# --- (2) the server-side render ---------------------------------------------------------------------
mkdir -p "$WORK/web/ssr"
cat >"$WORK/web/ssr/entry.tsx" <<'TSX'
import { writeFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import { StorageCard } from "../src/components/fleet/HealthStrip";
import { coreLinesView } from "../src/coreLines.ts";

const out = process.argv[2];
const now = Date.UTC(2026, 10, 19, 12, 0, 0);
const trend = (v: number) => [13, 14, 15, 16, 17, 18].map((d) => ({ day: `2026-11-${d}`, value: v }));
const answer = (basis: string | null) => ({
  diagnostics: [
    { key: "storage.free_bytes", name: "Free disk", value: 160_140_636_160, unit: "bytes", range: null, state: "no_range", basis: null, trend: trend(160e9) },
    { key: "storage.disk_fall", name: "Free disk lost a day", value: basis === null ? null : 7_239_039_662, unit: "bytes_per_day", range: null, state: basis === null ? "not_collected" : "no_range", basis, trend: trend(7e9) },
    { key: "storage.grew_most", name: "Store that grew most", store: "projects", value: 140_304_633, unit: "bytes_per_day", range: null, state: "no_range", basis: null, trend: trend(1.4e8) },
    { key: "storage.leaked_test_schemas", name: "Leaked test schemas", value: 141, unit: "count", range: { max: 200 }, state: "in", basis: null, trend: trend(141) },
  ],
});
const cases = [["day", "day"], ["fit", "fit"], ["none", null]] as const;
writeFileSync(
  out,
  JSON.stringify(
    cases.map(([name, basis]) => {
      const view = coreLinesView(answer(basis) as never, now);
      return {
        name,
        marks: view.diagnostics.map((m) => ({ name: m.name, value: m.value })),
        html: renderToStaticMarkup(<StorageCard storage={null} diagnostics={view.diagnostics} />),
      };
    }),
  ),
);
TSX
(cd "$WORK/web" && npx --no-install vite build --configLoader runner --ssr ssr/entry.tsx --outDir ssr-out) >"$WORK/ssr-build.out" 2>&1 ||
    harness "the server-side render of StorageCard does not build: $(grep -E 'error|Error|not exported|Could not resolve' "$WORK/ssr-build.out" | sed 's/\x1b\[[0-9;]*m//g' | head -n 3 | tr '\n' ' ' | head -c 400)"
entry=$(find "$WORK/web/ssr-out" -name 'entry.*js' | head -n 1)
[ -n "$entry" ] || harness "the SSR build wrote no entry"
cat >"$WORK/window.mjs" <<'JS'
const store = () => { const m = new Map(); return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k) }; };
globalThis.window = globalThis;
globalThis.location = new URL("http://127.0.0.1/");
globalThis.localStorage = store();
globalThis.sessionStorage = store();
globalThis.fetch = () => Promise.reject(new Error("no fetch in the render"));
JS
(cd "$WORK/web" && node --import "$WORK/window.mjs" "$entry" "$WORK/renders.json") >"$WORK/ssr-run.out" 2>&1 ||
    harness "the server-side render of StorageCard fails: $(grep -m 3 -E 'Error|at ' "$WORK/ssr-run.out" | tr '\n' ' ' | head -c 400)"

# --- (2)-(5) the rendered rows against the built CSS ------------------------------------------------
cat >"$WORK/check.mjs" <<'JS'
import { readFileSync } from "node:fs";
const [renders, cssList] = process.argv.slice(2);
const unescape = (s) => s.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&#x27;|&#39;/g, "'").replace(/&amp;/g, "&");
// The built CSS: every innermost rule; a selector of one class (`.x`, CSS
// escapes undone) gives that class's declarations, later rules last.
const css = readFileSync(cssList, "utf8").split("\n").filter(Boolean).map((f) => readFileSync(f, "utf8")).join("\n").replace(/\/\*[\s\S]*?\*\//g, "");
const byClass = new Map();
for (const m of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
  for (const sel of m[1].split(",").map((s) => s.trim())) {
    const one = /^\.((?:\\.|[\w-])+)$/.exec(sel);
    if (!one) continue;
    const name = one[1].replace(/\\(.)/g, "$1");
    const decls = byClass.get(name) ?? [];
    for (const d of m[2].split(";")) { const i = d.indexOf(":"); if (i > 0) decls.push([d.slice(0, i).trim(), d.slice(i + 1).trim()]); }
    byClass.set(name, decls);
  }
}
if (!byClass.has("flex")) { console.log("HARNESS the built CSS has no .flex rule: not the page's CSS"); process.exit(0); }
// What applies at some screen width: the element's unprefixed classes, and
// those whose every variant is a width (sm: ... 2xl:, max-sm: ...) or dark:.
// From sm up the card shares the row, so a width variant can meet a column
// narrower than a phone's. State variants (hover:, focus:) and print: are
// not read.
const phone = (c) => { const parts = c.split(":"); parts.pop(); return parts.every((v) => /^(max-)?(sm|md|lg|xl|2xl)$|^dark$/.test(v)); };
const applied = (cls) => cls.split(/\s+/).filter((c) => c && phone(c)).flatMap((c) => byClass.get(c) ?? []);
const nowrap = ([k, v]) => (k === "white-space" || k === "text-wrap-mode") && /nowrap/.test(v);
const resets = (decls) => decls.some(([k, v]) => (k === "white-space" || k === "text-wrap-mode") && !/nowrap/.test(v));
const around = (decls) => decls.filter(([k, v]) => nowrap([k, v]) || k === "text-overflow" && /ellipsis/.test(v) || /line-clamp$/.test(k)).map(([k, v]) => `${k}: ${v}`);
// Width buckets: below sm, then sm .. 2xl. A width variant applies at a
// bucket when its min-width breakpoint is at or below it (sm: from sm up), or
// its max-width one above it (max-sm: below sm). dark: applies in both.
const BPS = ["sm", "md", "lg", "xl", "2xl"];
const BUCKETS = ["below sm", "sm", "md", "lg", "xl", "2xl"];
const at_ = (c, b) => { const parts = c.split(":"); parts.pop(); return parts.every((x) => { if (x === "dark") return true; const m = /^(max-)?(sm|md|lg|xl|2xl)$/.exec(x); if (!m) return false; const k = BPS.indexOf(m[2]) + 1; return m[1] ? b < k : b >= k; }); };
const rank = (c) => { const parts = c.split(":"); parts.pop(); return parts.length === 0 ? 0 : 1 + Math.max(...parts.map((x) => { const m = /^(max-)?(sm|md|lg|xl|2xl)$/.exec(x); return m ? BPS.indexOf(m[2]) + (m[1] ? 0 : 5) : 0; })); };
const resolveAt = (cls, b) => {
  const st = {};
  for (const c of cls.split(/\s+/).filter((c) => c && at_(c, b)).sort((x, y) => rank(x) - rank(y))) for (const [k, v] of byClass.get(c) ?? []) st[k] = v;
  if (st["-webkit-line-clamp"] !== undefined && st["line-clamp"] === undefined) st["line-clamp"] = st["-webkit-line-clamp"];
  return st;
};
const clipsOf = (st, what) => [["overflow", st.overflow], ["overflow-x", st["overflow-x"]], ["text-overflow", st["text-overflow"]], ["line-clamp", st["line-clamp"]],
  ...(what === "name" ? [["white-space", st["white-space"]], ["text-wrap-mode", st["text-wrap-mode"]]] : [])]
  .filter(([k, v]) => v !== undefined && ((k === "overflow" || k === "overflow-x") ? /hidden|clip/.test(v) : k === "text-overflow" ? /ellipsis/.test(v) : k === "line-clamp" ? v !== "none" : /nowrap/.test(v))).map(([k, v]) => `${k}: ${v}`);
const aroundOf = (st) => [["white-space", st["white-space"]], ["text-wrap-mode", st["text-wrap-mode"]], ["text-wrap", st["text-wrap"]], ["text-overflow", st["text-overflow"]], ["line-clamp", st["line-clamp"]]]
  .filter(([k, v]) => v !== undefined && (k === "text-overflow" ? /ellipsis/.test(v) : k === "line-clamp" ? v !== "none" : /nowrap/.test(v))).map(([k, v]) => `${k}: ${v}`);
const clips = (decls, what) => decls.filter(([k, v]) =>
  (k === "overflow" || k === "overflow-x") && /hidden|clip/.test(v) ||
  k === "text-overflow" && /ellipsis/.test(v) ||
  what === "name" && (k === "white-space" || k === "text-wrap-mode") && /nowrap/.test(v)).map(([k, v]) => `${k}: ${v}`);
for (const r of JSON.parse(readFileSync(renders, "utf8"))) {
  const at = `(${r.name})`;
  const list = /<ul([^>]*aria-label="Storage diagnostics"[^>]*)>([\s\S]*?)<\/ul>/.exec(r.html);
  if (!list) { console.log(`FAIL (2) ${at} no "Storage diagnostics" list in the render`); continue; }
  const classOf = (attrs) => unescape((/class="([^"]*)"/.exec(attrs) ?? ["", ""])[1]);
  // The elements around each name: the card's root, the list, the row.
  const card = classOf((/^<div([^>]*)>/.exec(r.html) ?? ["", ""])[1]);
  const ul = classOf(list[1]);
  const rows = [...list[2].matchAll(/<li([^>]*)>([\s\S]*?)<\/li>/g)].map((m) => Object.assign([...m[2].matchAll(/<span([^>]*)>([\s\S]*?)<\/span>/g)].map((s) => ({
    cls: classOf(s[1]), text: unescape(s[2].replace(/<[^>]*>/g, "")),
  })), { li: classOf(m[1]) })).filter((spans) => spans.length >= 2);
  // (5) the words: one row per mark, coreLinesView's name and value word for word, in order
  if (rows.length !== r.marks.length) console.log(`FAIL (5) ${at} ${rows.length} rows for ${r.marks.length} diagnostics`);
  r.marks.forEach((mark, i) => {
    const row = rows[i];
    if (!row) return;
    const name = row.find((s) => s.text === mark.name);
    const value = row.find((s) => s.text === mark.value);
    if (!name) { console.log(`FAIL (2) ${at} no element of row ${i + 1} holds the whole name [${mark.name}] (the row says [${row.map((s) => s.text).join(" | ")}])`); return; }
    if (!value) { console.log(`FAIL (2) ${at} no element of row ${i + 1} holds the value [${mark.value}]`); return; }
    // The resolved style (R5, the lead's 09:49 ask): each property the
    // outcome depends on, resolved at each width bucket (below sm, sm, md,
    // lg, xl, 2xl) from the classes that apply there, later variants last.
    // The outcome is then judged per bucket: the name whole and shown, the
    // value whole and shown on the name's first line.
    const S = {
      name: (b) => resolveAt(name.cls, b), value: (b) => resolveAt(value.cls, b),
      row: (b) => resolveAt(row.li, b), list: (b) => resolveAt(ul, b), card: (b) => resolveAt(card, b),
    };
    const said = new Set();
    const fail = (key, msg) => { if (!said.has(key)) { said.add(key); console.log(msg); } };
    BUCKETS.forEach((bucket, b) => {
      const w = `at ${bucket}`;
      const n = S.name(b), v = S.value(b), li = S.row(b), l = S.list(b), c = S.card(b);
      // shown: no display none, visibility hidden/collapse or opacity 0 on the name, value or around them
      for (const [where, st] of [["name", n], ["value", v], ["row", li], ["list", l], ["card", c]]) {
        const got = [];
        if (st.display === "none") got.push("display: none");
        if (/^(hidden|collapse)$/.test(st.visibility ?? "")) got.push(`visibility: ${st.visibility}`);
        if (/^0(\.0*)?%?$/.test(st.opacity ?? "")) got.push(`opacity: ${st.opacity}`);
        if (got.length) fail(`shown-${where}`, `FAIL (3) ${at} [${mark.name}] is not shown ${w}: the ${where} sets ${got.join(", ")}`);
      }
      // the name is not clipped on its own element
      const cut = clipsOf(n, "name");
      if (cut.length) fail("cut-name", `FAIL (3) ${at} the name [${mark.name}] is cut on a narrow column ${w}: its element's CSS sets ${cut.join(", ")}`);
      // nothing around the name clips or stops it wrapping (strict: an inherited nowrap fails even if the name sets text-wrap back)
      for (const [where, st] of [["row", li], ["list", l], ["card", c]]) {
        const got = aroundOf(st, n);
        if (got.length) fail(`around-${where}`, `FAIL (3) ${at} the name [${mark.name}] is cut on a narrow column ${w}: the ${where} around it sets ${got.join(", ")}`);
      }
      // the name wraps: its own white-space/text-wrap, else the nearest ancestor's
      const ws = [n, li, l, c].find((st) => st["white-space"] !== undefined || st["text-wrap"] !== undefined || st["text-wrap-mode"] !== undefined) ?? {};
      const nowrapping = [["white-space", ws["white-space"]], ["text-wrap", ws["text-wrap"]], ["text-wrap-mode", ws["text-wrap-mode"]]]
        .filter(([k, x]) => x !== undefined && (k === "white-space" ? !/^(normal|pre-wrap|pre-line|break-spaces)$/.test(x) : /nowrap/.test(x))).map(([k, x]) => `${k}: ${x}`);
      if (nowrapping.length) fail("wrap", `FAIL (3) ${at} the name [${mark.name}] does not wrap on a narrow column ${w}: it gets ${nowrapping.join(", ")}`);
      // the value on the name's first line: a flex row, row direction, no wrap, baseline/start alignment, no align-self moving it
      if (li.display !== "flex") fail("row-flex", `FAIL (4) ${at} the row of [${mark.name}] is not a flex row ${w} (display: ${li.display ?? "unset"})`);
      if (li["flex-direction"] !== undefined && !/^(row|row-reverse)$/.test(li["flex-direction"])) fail("row-dir", `FAIL (4) ${at} the row of [${mark.name}] is a column ${w} (flex-direction: ${li["flex-direction"]}): the value sits under the name`);
      if (li["flex-wrap"] !== undefined && li["flex-wrap"] !== "nowrap") fail("row-wrap", `FAIL (4) ${at} the row of [${mark.name}] wraps ${w} (flex-wrap: ${li["flex-wrap"]}): the value can fall below the name`);
      if (li["align-items"] !== undefined && !/^(baseline|first baseline|flex-start|start|self-start|stretch|normal)$/.test(li["align-items"])) fail("row-align", `FAIL (4) ${at} the value of [${mark.name}] leaves the name's first line ${w} (the row's align-items: ${li["align-items"]})`);
      if (v["align-self"] !== undefined && !/^(auto|baseline|first baseline|flex-start|start|self-start|stretch|normal)$/.test(v["align-self"])) fail("value-align", `FAIL (4) ${at} the value [${mark.value}] leaves the name's first line ${w} (align-self: ${v["align-self"]})`);
      // the value whole: not clipped, does not shrink
      const vcut = clipsOf(v, "value");
      if (vcut.length) fail("cut-value", `FAIL (4) ${at} the value [${mark.value}] can be cut ${w}: ${vcut.join(", ")}`);
      if (v["flex-shrink"] !== "0") fail("value-shrink", `FAIL (4) ${at} the value [${mark.value}] shrinks beside the name ${w} (flex-shrink: ${v["flex-shrink"] ?? "unset"})`);
    });
    if (r.name === "day") {
      const t = (st, keys) => keys.map((k) => `${k}=${st[k] ?? "-"}`).join(" ");
      console.log(`TABLE (day) row ${i + 1} below sm: row ${t(S.row(0), ["display", "flex-direction", "flex-wrap", "align-items"])} | name ${t(S.name(0), ["white-space", "text-wrap", "overflow", "text-overflow", "display", "visibility", "opacity"])} | value ${t(S.value(0), ["flex-shrink", "align-self", "overflow", "text-overflow", "display", "opacity"])}`);
    }
  });
  const fall = r.marks.find((m) => m.name.startsWith("Free disk lost a day"));
  console.log(`OK ${at} ${rows.length} rows; the disk fall named [${fall ? fall.name : "none"}]`);
}
JS
node "$WORK/check.mjs" "$WORK/renders.json" "$WORK/css.list" >"$WORK/check.out" 2>&1 ||
    harness "the render check did not run: $(head -c 300 "$WORK/check.out")"
! grep -q '^HARNESS ' "$WORK/check.out" || harness "$(sed -n 's/^HARNESS //p' "$WORK/check.out" | head -n 1)"
grep '^FAIL ' "$WORK/check.out" | sed 's/^FAIL //' >"$WORK/fails" || true
while IFS= read -r line; do check_failed "$line"; done <"$WORK/fails"
[ "$(grep -c '^OK ' "$WORK/check.out")" -eq 3 ] || harness "the render check read $(grep -c '^OK ' "$WORK/check.out") renders, not 3"
# 225's words are the subject: the "day" and "fit" renders must carry them, or (3) checked nothing new.
grep -q "^OK (day) .*\[Free disk lost a day (the last day's fall)\]" "$WORK/check.out" ||
    harness "the day render's disk fall is not named with 225's basis words: $(grep '^OK (day)' "$WORK/check.out")"
grep -q "^OK (fit) .*\[Free disk lost a day (the week's fit)\]" "$WORK/check.out" ||
    harness "the fit render's disk fall is not named with 225's basis words: $(grep '^OK (fit)' "$WORK/check.out")"
sed -n 's/^OK /227: (2)-(5) checked: /p' "$WORK/check.out" >&2
sed -n 's/^TABLE /227: resolved: /p' "$WORK/check.out" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "227: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "227: each storage diagnostic's name wraps whole on a narrow column, 225's basis words included; the value stays whole"
exit 0
