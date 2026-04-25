import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// https://vitejs.dev/config/ — adjusted for Tauri per the docs at
// https://v2.tauri.app/start/frontend/vite/.
//
// PR-α (phase 62) added React + Tailwind v4 plugins. Tailwind v4 is
// CSS-first (`@import "tailwindcss"` lives in src/styles.css), so no
// `tailwind.config.js` exists at the project root.
export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],
  // Prevent Vite from obscuring Rust errors in the dev console.
  clearScreen: false,
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
