/**
 * React 19 root mount.
 *
 * Wraps `<App />` in `<BrowserRouter>` (react-router v7) and mounts a
 * `sonner` `<Toaster />` so the rest of the React tree can call
 * `toast(...)` without re-providing context.
 *
 * `styles.css` carries the Tailwind v4 import + `@theme` block; it
 * has to be imported here (not from a leaf component) so the JIT
 * walks the entire bundle's class set.
 */

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { Toaster } from "sonner";

import App from "./App";
import "./styles.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("Missing #root container in index.html");
}

createRoot(container).render(
  <StrictMode>
    <BrowserRouter>
      <App />
      <Toaster richColors position="bottom-right" />
    </BrowserRouter>
  </StrictMode>,
);
