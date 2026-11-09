// The WCAG contrast of every text/background pair of Spawn's theme tokens,
// in both themes: `node scripts/contrast.mjs [src/styles/spawn.css]`. It reads
// every rule whose selector list names `[data-theme="light"]` or
// `[data-theme="dark"]` (later declarations win, as in the cascade), resolves
// `var(--x)`, converts oklch or hex to sRGB, and exits 1 when a pair is under
// its minimum: 4.5:1 for text, 3:1 for marks, icons and the focus ring
// (WCAG 2.2 AA, 1.4.3 and 1.4.11). vite.config.ts runs it at the start of
// every build, so a token change that drops a pair under AA fails the build.
import { readdirSync, readFileSync } from "node:fs";

const THEMES = ["light", "dark"];
// [text, background, minimum, role]
const PAIRS = [
  ["ink", "page", 4.5, "body text on the page"],
  ["ink", "surface", 4.5, "body text on a card"],
  ["ink", "inset", 4.5, "body text on an inset"],
  ["ink", "selected", 4.5, "body text on the selected row"],
  ["ink-2", "page", 4.5, "secondary text on the page"],
  ["ink-2", "surface", 4.5, "secondary text on a card"],
  ["ink-2", "hover", 4.5, "secondary text on a hovered row"],
  ["ink-2", "selected", 4.5, "secondary text on the selected row"],
  ["ink-3", "surface", 4.5, "faint text (metadata) on a card"],
  ["ink-3", "page", 4.5, "faint text on the page"],
  ["ink-3", "selected", 4.5, "faint text on the selected row"],
  ["accent-fg", "accent", 4.5, "primary button label"],
  ["accent-ink", "surface", 4.5, "accent text/link on a card"],
  ["accent", "surface", 3, "focus ring and accent mark on a card"],
  ["line-strong", "surface", 1.3, "strong hairline (decorative, informative only with text)"],
  ["ok-ink", "ok-tint", 4.5, "ok status text on its tint"],
  ["ok-ink", "surface", 4.5, "ok status text on a card"],
  ["wait-ink", "wait-tint", 4.5, "attention text on its tint"],
  ["wait-ink", "surface", 4.5, "attention text on a card"],
  ["fail-ink", "fail-tint", 4.5, "failed text on its tint"],
  ["fail-ink", "surface", 4.5, "failed text on a card"],
  ["idle-ink", "idle-tint", 4.5, "idle text on its tint"],
  ["info-ink", "info-tint", 4.5, "info text on its tint"],
  ["judge-ink", "judge-tint", 4.5, "model judge text on its tint"],
  ["ok-mark", "surface", 3, "ok dot/icon on a card"],
  ["wait-mark", "surface", 3, "attention dot/icon on a card"],
  ["fail-mark", "surface", 3, "failed dot/icon on a card"],
  ["idle-mark", "surface", 3, "idle dot/icon on a card"],
  ["ok-mark", "selected", 3, "ok dot on the selected row"],
  ["fail-mark", "selected", 3, "failed dot on the selected row"],
  ["trace-execution", "surface", 3, "trace span: execution"],
  ["trace-turn", "surface", 3, "trace span: turn"],
  ["trace-tool", "surface", 3, "trace span: tool"],
  ["trace-command", "surface", 3, "trace span: command"],
  ["chart-line", "surface", 3, "chart line"],
  ["judge-ink", "surface", 4.5, "model judge text on a card"],
  ["info-ink", "surface", 4.5, "working/info text on a card"],
  // The names the views use (aliases of the roles above).
  ["foreground", "page-canvas", 4.5, "text-foreground on the page"],
  ["muted-foreground", "surface-hover", 4.5, "text-muted-foreground on a hovered row"],
  ["faint-foreground", "surface-hover", 4.5, "text-faint-foreground on a hovered row"],
  ["faint-foreground", "sidebar", 4.5, "text-faint-foreground in the sidebar"],
  ["muted-foreground", "sidebar-accent", 4.5, "text-muted-foreground on the current sidebar row"],
  ["success", "surface-hover", 4.5, "text-success on a hovered row"],
  ["warning", "surface-hover", 4.5, "text-warning on a hovered row"],
  ["destructive", "surface-hover", 4.5, "text-destructive on a hovered row"],
  ["primary-foreground", "primary", 4.5, "primary button label"],
  ["tooltip-foreground", "tooltip", 4.5, "tooltip text"],
  ["terminal-foreground", "terminal", 4.5, "terminal text"],
  ["muted-foreground", "fail-tint", 4.5, "open-alerts band: alert text"],
  ["faint-foreground", "fail-tint", 4.5, "open-alerts band: alert age"],
  ["muted-foreground", "wait-tint", 4.5, "secondary text on an attention tint"],
  ["faint-foreground", "inset", 4.5, "metadata in an inset (bg-muted)"],
];

// A terminal is dark in both themes: Terminal.tsx gives every terminal
// surface data-theme="dark", so the page's text tokens inside it (a
// header's title, tag and status) resolve in the dark theme whatever the
// page's is. These are those tokens on the terminal's ground, read in the
// dark theme; with the light page's tokens they fail, which is what an
// unscoped terminal showed (the light theme's dark ink on a dark header).
const TERMINAL_THEME = "dark";
const TERMINAL_PAIRS = [
  ["foreground", "terminal", 4.5, "a terminal's title"],
  ["muted-foreground", "terminal", 4.5, "a terminal's tag"],
  ["faint-foreground", "terminal", 4.5, "a terminal's status line"],
  ["ok-ink", "terminal", 4.5, "a terminal's live badge"],
];

function oklchToSrgb(L, C, H) {
  const h = (H * Math.PI) / 180, a = C * Math.cos(h), b = C * Math.sin(h);
  const l_ = L + 0.3963377774 * a + 0.2158037573 * b;
  const m_ = L - 0.1055613458 * a - 0.0638541728 * b;
  const s_ = L - 0.0894841775 * a - 1.291485548 * b;
  const l = l_ ** 3, m = m_ ** 3, s = s_ ** 3;
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ]; // linear sRGB
}
const clamp = (x) => Math.min(1, Math.max(0, x));
function linear(value) {
  const ok = /oklch\(\s*([\d.]+)\s+([\d.]+)\s+([\d.]+)\s*\)/.exec(value);
  if (ok) {
    const lin = oklchToSrgb(+ok[1], +ok[2], +ok[3]);
    const out = lin.some((x) => x < -0.001 || x > 1.001);
    return { lin: lin.map(clamp), out };
  }
  const hex = /#([0-9a-f]{6})/i.exec(value);
  if (hex) {
    const n = parseInt(hex[1], 16);
    const lin = [n >> 16, (n >> 8) & 255, n & 255].map((c) => {
      c /= 255;
      return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
    });
    return { lin, out: false };
  }
  return null;
}
const luminance = ([r, g, b]) => 0.2126 * r + 0.7152 * g + 0.0722 * b;
const ratio = (a, b) => {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
};

const css = readFileSync(process.argv[2] ?? new URL("../src/styles/spawn.css", import.meta.url), "utf8");
let failed = 0;
const rules = [...css.replace(/\/\*[\s\S]*?\*\//g, "").matchAll(/([^{}]+)\{([^{}]*)\}/g)];
const resolverFor = (theme) => {
  const tokens = {};
  for (const [, selector, block] of rules) {
    if (!selector.includes(`[data-theme="${theme}"]`)) continue;
    for (const [, name, value] of block.matchAll(/--([\w-]+):\s*([^;]+);/g)) tokens[name] = value.trim();
  }
  const resolve = (name, depth = 0) => {
    const v = tokens[name];
    if (!v) return null;
    const ref = /^var\(--([\w-]+)\)$/.exec(v);
    return ref && depth < 5 ? resolve(ref[1], depth + 1) : v;
  };
  return resolve;
};
const check = (heading, resolve, pairs) => {
  console.log(`\n${heading}`);
  for (const [fg, bg, min, role] of pairs) {
    const a = resolve(fg) && linear(resolve(fg)), b = resolve(bg) && linear(resolve(bg));
    if (!a || !b) { console.log(`  ?    ${fg} on ${bg}: missing`); failed++; continue; }
    const r = ratio(a.lin, b.lin);
    const ok = r >= min;
    if (!ok) failed++;
    console.log(`  ${ok ? "ok  " : "FAIL"} ${r.toFixed(2).padStart(5)}:1 (min ${min})  ${fg} on ${bg}  ${role}${a.out || b.out ? "  [out of sRGB gamut, clamped]" : ""}`);
  }
};
for (const theme of THEMES) check(theme, resolverFor(theme), PAIRS);
check(`inside a terminal (the ${TERMINAL_THEME} theme, on either page)`, resolverFor(TERMINAL_THEME), TERMINAL_PAIRS);

// And every terminal surface is scoped so: a component that paints
// bg-terminal sets data-theme="dark" on it, as often as it paints it.
const srcDir = new URL("../src/", import.meta.url);
const tsx = (dir) =>
  readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory()
      ? tsx(new URL(`${entry.name}/`, dir))
      : entry.name.endsWith(".tsx") ? [new URL(entry.name, dir)] : [],
  );
console.log("\nterminal surfaces carry the dark theme");
for (const file of tsx(srcDir)) {
  const text = readFileSync(file, "utf8");
  const painted = (text.match(/\bbg-terminal\b(?!-)/g) ?? []).length;
  if (!painted) continue;
  const scoped = (text.match(/data-theme="dark"/g) ?? []).length;
  const ok = scoped >= painted;
  if (!ok) failed++;
  const name = file.pathname.slice(srcDir.pathname.length);
  console.log(`  ${ok ? "ok  " : "FAIL"} ${name}: ${painted} bg-terminal, ${scoped} data-theme="dark"`);
}
console.log(failed ? `\n${failed} pair(s) below their minimum` : "\nevery pair meets its minimum");
process.exit(failed ? 1 : 0);
