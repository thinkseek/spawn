#!/bin/sh
# Objective 119 [feature]: affectUI's primitives wear Spawn's theme. The
# foundation of the affectUI series (the owner via lead-mac, 11-13 14:02).
#
# The owner's push a9cf34c1 put affectUI's 20 free primitives under
# web/src/components/ui/<name>.tsx byte for byte; the lead's kit commit
# (the owner's kit, 11-13 15:00) replaces six of them with the kit's -clean
# variants at the same paths (badge, button, card, empty-state, skeleton,
# tooltip) and adds kpi-chart: 21 files, with
# web/src/components/ui/affectui.sha256. Nothing imports them yet, and each reads its
# colours, radii, shadows and fonts from `--aui-*` custom properties that
# nothing defines, so each would fall back to affectUI's own palette, not
# Spawn's.
#
# Required: the 21 files stay byte-identical, alone in their directory;
# every `--aui-*` name they read is set by Spawn's own stylesheet from the
# theme roles of web/src/styles/spawn.css, for light and dark alike, with no
# literal colour; and that stylesheet is in the built app. Spawn's own
# wrappers live outside components/ui/ and import each file by its full path.
#
# Black box over the tree under test (the working directory) and its build:
#   (1) web/src/components/ui/ holds exactly the 21 files below, each with
#       the sha256 below, and affectui.sha256: nothing changed, nothing
#       added (no 21st file, no index file).
#   (2) Every `--aui-<name>` the 21 files read is declared in
#       web/src/styles/aui.css in a rule whose selectors name both
#       [data-theme="light"] and [data-theme="dark"]; each value has no
#       literal colour (no #, rgb, hsl, oklch, lab, color-mix), and every
#       custom property it uses is declared by Spawn's styles
#       (web/src/styles/*.css other than aui.css, or web/src/index.css).
#   (3) The UI builds (`npm run build`, tsc then vite, into a directory of
#       the scenario's own) and the built CSS declares every one of those
#       names.
#   (4) The radii, by what reads them: --aui-radius-sm (the chips) resolves
#       to spawn.css's --radius-chip, --aui-radius (the controls) to
#       --radius-control, --aui-radius-lg (cards, overlays) to --radius-card.
#   (5) Contrast, in each theme: every hue or status name the files may set
#       as text (ink, muted, info, success, warning, danger, blue, orange,
#       teal, red, green, purple), resolved through aui.css and Spawn's
#       roles, is at least 4.5:1 on --aui-surface and on --aui-subtle
#       (WCAG 2.2 AA for text, as Spawn's scripts/contrast.mjs computes it).
#
# Needs node and the controller's web/node_modules (as check-web).
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
# needs: controller:web/node_modules/

set -eu

asserted=""
harness() {
    echo "119: harness: $*" >&2
    exit 2
}
controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test is the working directory: the controller's copy holds
# main's web/src, never the candidate's.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -d "$controller/web/node_modules" ] || harness "no node_modules in $controller/web"
for tool in node npm sha256sum find sort; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
finish() {
    rc=$?
    trap - EXIT
    set +e
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "119: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM
: >"$WORK/failures"
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

# The 21 files as the owner's kit commit lands them, by sha256.
cat >"$WORK/pinned" <<'PIN'
d5e6309addea4d8a9d65e4c58abd1075ed85bef43f6e65812e55a9918d3b234f  badge.tsx
686046d5fbe47b1d2234980c05d4fc56d2ad69ee3a084de8583c82759a7d4acb  button.tsx
71e8b374ab0faabb133f5058b6415ee5086352db87e712ffc457dec0b8c4a1cc  card.tsx
3077adb840b09f73d6af5236d17c3db9752150791504cfce633a4e88969bef4c  code-block.tsx
2dc8e3cb724dcfb3b8394e3e2ec5632613f3e042e55c4298450f89879fb1928a  dialog.tsx
495f16a093feb816c8d1f26f9f4adeecfb95712fbf2ee55f9961432de9fe4d46  empty-state.tsx
641b06e265e47347431d542ae7847ce05d22791ca2b37acbc4c8afd444177ba4  kbd.tsx
81a7e5beb60e835413d35d99b08e3a5518fbcfe15a557a8d83a79663ec16000d  kpi-chart.tsx
b5136de9b6a800d534ba5d0f9e928273bf9a230d4b358f484f8c4e71ed250551  notification-stack.tsx
c56e7605203d19041fa33c6f177bccd4a02bf75b7923ba037461fd64f8440a6d  number-ticker.tsx
c01b75849cbc2f71e966448723d085fe75e1819f89da83b1aece6daf593a852e  pagination.tsx
a31e284aeb0c783b569425eeb087aa542415256d8d98bff4c9417701ba7d72ee  popover.tsx
d2f2d32c2ef7f08834de472162f2c4a4ab04c94b26e54b8491fd516ffdf406df  progress-tracker.tsx
6e4303382350f70b232e451816113855311512e7197410984bac4a764064f730  segmented-control.tsx
9caad90bfa8e9f6d6ef1f544d81486cef0f9ef427c48c1e3da34295ce5faca09  select.tsx
65c0f129115f0e95421fd62be17451b20d82e80199d81a2bd6d7e597c7c60acd  separator.tsx
1664e4f6df2a4965b9f9cc483e69ba5f445bbde257c45b8c0f13b61784efb0e6  skeleton.tsx
34fab447f22df019280f53dc5e59f61ef036e97433531b9d3acc6ef93ba2b1dc  spinner.tsx
1ddf35a0ed451bfcf2c53715d0dcb90ac15321f32db8d3b369534743c3e045d1  toggle-group.tsx
91ab651c72b613be544e2b2e83b557e2f522c9a6c458d05ade85365086865766  tool-call-card.tsx
248d67efb0b588fb949990c6427fc9d7aea734056eadddca564a3da7269a44f1  tooltip.tsx
PIN
ui=$tree/web/src/components/ui
[ -d "$ui" ] || harness "no web/src/components/ui in the tree"

# --- (1) the 21 files, unchanged and alone ----------------------------------------------
(cd "$ui" && sha256sum -c --quiet "$WORK/pinned") >"$WORK/sum.out" 2>&1 ||
    check_failed "(1) affectUI's files differ from the pinned sha256: $(head -c 400 "$WORK/sum.out" | tr '\n' ' ')"
{ awk '{print $2}' "$WORK/pinned"; echo affectui.sha256; } | sort >"$WORK/expected"
(cd "$ui" && find . -mindepth 1 -maxdepth 1 | sed 's#^\./##' | sort) >"$WORK/present"
extra=$(comm -13 "$WORK/expected" "$WORK/present" | tr '\n' ' ')
missing=$(comm -23 "$WORK/expected" "$WORK/present" | tr '\n' ' ')
[ -z "$extra" ] || check_failed "(1) web/src/components/ui holds files that are not affectUI's 21: $extra"
[ -z "$missing" ] || check_failed "(1) web/src/components/ui lacks: $missing"
echo "119: (1) checked: the 21 files, unchanged and alone" >&2

# --- (2) every --aui- name set from Spawn's roles, for both themes -------------------------
cat >"$WORK/tokens.mjs" <<'JS'
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
const [web] = process.argv.slice(2);
const ui = join(web, "src/components/ui");
const used = new Set();
for (const name of readdirSync(ui).filter((n) => n.endsWith(".tsx"))) {
  for (const m of readFileSync(join(ui, name), "utf8").matchAll(/var\((--aui-[a-z0-9-]+)/g)) used.add(m[1]);
}
const strip = (css) => css.replace(/\/\*[\s\S]*?\*\//g, "");
// Top-level rules only (an @media or @theme block is read as its own
// selector): selector -> declarations.
function rules(css) {
  const out = [];
  const re = /([^{}]+)\{([^{}]*)\}/g;
  for (const m of strip(css).matchAll(re)) {
    const decls = new Map();
    for (const d of m[2].split(";")) {
      const i = d.indexOf(":");
      if (i > 0) decls.set(d.slice(0, i).trim(), d.slice(i + 1).trim());
    }
    out.push({ selector: m[1].trim(), decls });
  }
  return out;
}
console.log(`USED ${[...used].sort().join(" ")}`);
let aui;
try { aui = readFileSync(join(web, "src/styles/aui.css"), "utf8"); }
catch { console.log("FAIL (2) web/src/styles/aui.css is missing"); process.exit(0); }
const spawnFiles = readdirSync(join(web, "src/styles")).filter((n) => n.endsWith(".css") && n !== "aui.css")
  .map((n) => join(web, "src/styles", n)).concat(join(web, "src/index.css"));
const spawnDeclared = new Set();
for (const f of spawnFiles) for (const r of rules(readFileSync(f, "utf8"))) for (const k of r.decls.keys()) spawnDeclared.add(k);
const colour = /#[0-9a-f]{3,8}\b|\b(rgba?|hsla?|oklch|oklab|lab|lch|color-mix|hwb)\(/i;
const auiRules = rules(aui);
for (const name of [...used].sort()) {
  const defs = auiRules.filter((r) => r.decls.has(name));
  const themes = (r) => ({ light: /\[data-theme="?light"?\]/.test(r.selector), dark: /\[data-theme="?dark"?\]/.test(r.selector) });
  if (!defs.some((r) => themes(r).light) || !defs.some((r) => themes(r).dark)) {
    console.log(`FAIL (2) ${name} is not declared in aui.css under both [data-theme="light"] and [data-theme="dark"]`);
    continue;
  }
  for (const r of defs) {
    const value = r.decls.get(name);
    if (colour.test(value)) console.log(`FAIL (2) ${name} has a literal colour in aui.css: ${value}`);
    for (const m of value.matchAll(/var\((--[a-z0-9-]+)/g)) {
      if (!spawnDeclared.has(m[1])) console.log(`FAIL (2) ${name} uses ${m[1]}, which Spawn's styles do not declare`);
    }
  }
}

// (4) and (5): names resolved per theme. Spawn's rules first, aui.css last
// (index.css imports it after spawn.css); a later declaration wins.
const ordered = spawnFiles.map((f) => rules(readFileSync(f, "utf8"))).flat().concat(auiRules);
const resolverFor = (theme) => {
  const tokens = new Map();
  for (const r of ordered) {
    const applies = theme === null ? true : r.selector.includes(`[data-theme="${theme}"]`) || /(^|\s)@theme\b/.test(r.selector);
    if (!applies) continue;
    for (const [k, v] of r.decls) if (k.startsWith("--")) tokens.set(k, v);
  }
  const resolve = (name, depth = 0) => {
    const v = tokens.get(name);
    if (v === undefined) return null;
    const ref = /^var\((--[\w-]+)\s*(?:,[^)]*)?\)$/.exec(v);
    return ref && depth < 8 ? resolve(ref[1], depth + 1) : v;
  };
  return resolve;
};
for (const [aui, spawn] of [["--aui-radius-sm", "--radius-chip"], ["--aui-radius", "--radius-control"], ["--aui-radius-lg", "--radius-card"]]) {
  for (const theme of ["light", "dark"]) {
    const resolve = resolverFor(theme);
    const got = resolve(aui), want = resolve(spawn);
    if (want === null) { console.log(`HARNESS spawn.css declares no ${spawn}`); continue; }
    if (got !== want) console.log(`FAIL (4) ${aui} is ${got ?? "not declared"} in the ${theme} theme, not ${spawn} (${want})`);
  }
}
function oklchToLinear(L, C, H) {
  const h = (H * Math.PI) / 180, a = C * Math.cos(h), b = C * Math.sin(h);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ].map((x) => Math.min(1, Math.max(0, x)));
}
function linear(value) {
  const ok = /oklch\(\s*([\d.]+)(%?)\s+([\d.]+)\s+([\d.]+)\s*\)/.exec(value ?? "");
  if (ok) return oklchToLinear(ok[2] ? +ok[1] / 100 : +ok[1], +ok[3], +ok[4]);
  const hex = /^#([0-9a-f]{6})$/i.exec((value ?? "").trim());
  if (hex) {
    const n = parseInt(hex[1], 16);
    return [n >> 16, (n >> 8) & 255, n & 255].map((c) => (c /= 255) <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  }
  return null;
}
const lum = ([r, g, b]) => 0.2126 * r + 0.7152 * g + 0.0722 * b;
const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };
const TEXT = ["ink", "muted", "info", "success", "warning", "danger", "blue", "orange", "teal", "red", "green", "purple"];
for (const theme of ["light", "dark"]) {
  const resolve = resolverFor(theme);
  for (const ground of ["--aui-surface", "--aui-subtle"]) {
    const bg = linear(resolve(ground));
    if (!bg) { console.log(`FAIL (5) ${ground} does not resolve to a colour in the ${theme} theme: ${resolve(ground)}`); continue; }
    for (const t of TEXT) {
      const name = `--aui-${t}`;
      if (!used.has(name)) continue;
      const fg = linear(resolve(name));
      if (!fg) { console.log(`FAIL (5) ${name} does not resolve to a colour in the ${theme} theme: ${resolve(name)}`); continue; }
      const r = ratio(fg, bg);
      if (r < 4.5) console.log(`FAIL (5) ${name} on ${ground} in the ${theme} theme is ${r.toFixed(2)}:1, under 4.5:1`);
      else console.log(`OK (5) ${name} on ${ground} ${theme} ${r.toFixed(2)}`);
    }
  }
}
JS
node "$WORK/tokens.mjs" "$tree/web" >"$WORK/tokens.out" 2>&1 || harness "the token check did not run: $(head -c 300 "$WORK/tokens.out")"
! grep -q '^HARNESS ' "$WORK/tokens.out" || harness "$(sed -n 's/^HARNESS //p' "$WORK/tokens.out" | head -n 3)"
grep '^FAIL ' "$WORK/tokens.out" | sed 's/^FAIL //' | while IFS= read -r line; do check_failed "$line"; done
[ "$(grep -c '^OK (5) ' "$WORK/tokens.out")" -ge 4 ] || grep -q '^FAIL ' "$WORK/tokens.out" ||
    harness "(5) measured no pair: $(head -c 300 "$WORK/tokens.out")"
used=$(sed -n 's/^USED //p' "$WORK/tokens.out")
[ -n "$used" ] || harness "the files read no --aui- name: $(head -c 300 "$WORK/tokens.out")"
echo "119: (2) checked: $(echo "$used" | wc -w | tr -d ' ') --aui- names" >&2
echo "119: (4) checked: the radii; (5) checked: $(grep -c '^OK (5) ' "$WORK/tokens.out") text pairs at 4.5:1 or more" >&2

# --- (3) the build, and the names in its CSS ------------------------------------------------
cp -a "$tree/web" "$WORK/web"
rm -rf "$WORK/web/node_modules" "$WORK/web/dist"
ln -s "$controller/web/node_modules" "$WORK/web/node_modules"
if (cd "$WORK/web" && npm run build -- --configLoader runner --outDir "$WORK/dist") >"$WORK/build.out" 2>&1; then
    cat "$WORK/dist"/assets/*.css >"$WORK/built.css" 2>/dev/null || harness "the build wrote no CSS"
    for name in $used; do
        grep -q -- "$name:" "$WORK/built.css" || check_failed "(3) the built CSS does not declare $name"
    done
    echo "119: (3) checked: the UI builds and its CSS declares them" >&2
else
    check_failed "(3) the UI does not build: $(grep -E 'error|Error' "$WORK/build.out" | head -n 5 | tr '\n' ' ' | head -c 500)"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "119: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "119: affectUI's 21 files are unchanged and alone, and wear Spawn's theme in both modes"
exit 0
