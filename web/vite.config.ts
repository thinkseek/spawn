import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // Inline, so vite never loads a postcss.config.js or .postcssrc from the
  // tree (deploy.sh builds the UI with `--config vite.config.ts`, so no
  // vite.config.js is loaded either). Tailwind runs as the plugin above.
  css: { postcss: {} },
});
