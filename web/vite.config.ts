import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";

// Every build checks the theme's text/background pairs against WCAG AA
// (scripts/contrast.mjs) before anything is bundled, and fails on a pair
// below its minimum, so `npm run build` and the gate's check-web refuse a
// palette that is hard to read. A plugin rather than a package.json script:
// package.json is the dependency set check-web guards.
function contrast(): Plugin {
  return {
    name: "spawn-contrast",
    apply: "build",
    buildStart() {
      const script = fileURLToPath(new URL("./scripts/contrast.mjs", import.meta.url));
      try {
        execFileSync(process.execPath, [script], { encoding: "utf8" });
      } catch (e) {
        const report = String((e as { stdout?: string }).stdout ?? (e as Error).message);
        const failing = report.split("\n").filter((l) => /^\s+(FAIL|\?)|below their minimum/.test(l));
        throw new Error(`theme contrast check failed (node scripts/contrast.mjs):\n${failing.join("\n")}`);
      }
    },
  };
}

export default defineConfig({
  plugins: [contrast(), react(), tailwindcss()],
  // Inline, so vite never loads a postcss.config.js or .postcssrc from the
  // tree (deploy.sh builds the UI with `--config vite.config.ts`, so no
  // vite.config.js is loaded either). Tailwind runs as the plugin above.
  css: { postcss: {} },
});
