import { defineConfig } from "vite";

// https://vitejs.dev/config/ — adjusted for Tauri per the docs at
// https://v2.tauri.app/start/frontend/vite/.
export default defineConfig(async () => ({
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
