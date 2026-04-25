/// <reference types="vite/client" />

// Build-time constants injected by Vite's `define` (see vite.config.ts).
// Both are baked at bundle time; the runtime never sees `package.json`
// or `process`.
declare const __APP_VERSION__: string;
declare const __APP_BUILD__: string;
