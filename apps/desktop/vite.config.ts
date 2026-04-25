import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// https://vitejs.dev/config/ — adjusted for Tauri per the docs at
// https://v2.tauri.app/start/frontend/vite/.
//
// PR-α (phase 62) added React + Tailwind v4 plugins. Tailwind v4 is
// CSS-first (`@import "tailwindcss"` lives in src/styles.css), so no
// `tailwind.config.js` exists at the project root.
//
// PR-δ (phase 66) adds the `__APP_VERSION__` / `__APP_BUILD__` define
// constants the Settings → About tab renders. Reading from
// `package.json` keeps the version in one place; the build stamp is
// the build's wall-clock so a "version 0.1.0 / build <date>" pair is
// unique even when the version doesn't bump.
const pkg = JSON.parse(
  readFileSync(fileURLToPath(new URL("./package.json", import.meta.url)), "utf-8"),
) as { version: string };

export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],
  // Prevent Vite from obscuring Rust errors in the dev console.
  clearScreen: false,
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
    __APP_BUILD__: JSON.stringify(new Date().toISOString().slice(0, 10)),
  },
  server: {
    port: 1420,
    strictPort: true,
    host: undefined,
    hmr: undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
}));
