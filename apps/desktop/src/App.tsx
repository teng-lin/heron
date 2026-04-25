/**
 * Top-level route switch.
 *
 * - `/` — placeholder redirect to `/onboarding`. Real first-run
 *   detection (read `Settings`, branch on `vault_root` being empty)
 *   lands in PR-δ.
 * - `/onboarding` — five-step walkthrough stub (PR-α).
 * - `/home` — dashboard with the `heron_status` smoke test +
 *   "Start recording" entry point (PR-β).
 * - `/recording` — full-window in-progress recording view (PR-β).
 * - `/review/:sessionId` — review UI stub.
 * - `/salvage` — crash-recovery list (PR-η).
 * - `/settings` — settings form stub.
 * - `*` — anything else falls back to `/`. Without this, navigating
 *   to a typo or stale link renders a blank screen rather than
 *   landing back at the onboarding redirect.
 *
 * The `<ConsentGate />` modal is mounted at the shell level (rather
 * than inside `<Home />`) because the consent flow needs to survive
 * route changes — e.g. a future global hotkey that opens the gate
 * from any page.
 *
 * `useTrayNav()` hooks the menubar tray's "Settings…" / `nav:*`
 * events into `react-router`'s `useNavigate`.
 *
 * `useSalvagePrompt()` runs once on mount: scans the cache for
 * unfinalized sessions and pops a Sonner banner with a "Open Salvage"
 * action button when there are any. The auto-redirect is intentionally
 * NOT done — the user might want to use the app for something else
 * first.
 */

import { useEffect } from "react";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";
import { toast } from "sonner";

import ConsentGate from "./components/ConsentGate";
import { useTrayNav } from "./hooks/useTrayNav";
import { invoke } from "./lib/invoke";
import Home from "./pages/Home";
import Onboarding from "./pages/Onboarding";
import Recording from "./pages/Recording";
import Review from "./pages/Review";
import Salvage from "./pages/Salvage";
import Settings from "./pages/Settings";
import { useSalvagePromptStore } from "./store/salvage";

export default function App() {
  useTrayNav();
  useSalvagePrompt();

  return (
    <>
      <Routes>
        <Route path="/" element={<Navigate to="/onboarding" replace />} />
        <Route path="/home" element={<Home />} />
        <Route path="/onboarding" element={<Onboarding />} />
        <Route path="/recording" element={<Recording />} />
        <Route path="/review/:sessionId" element={<Review />} />
        <Route path="/salvage" element={<Salvage />} />
        <Route path="/settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
      <ConsentGate />
    </>
  );
}

/**
 * On app mount, scan the cache for unfinalized sessions; if any are
 * found, pop a Sonner banner with an action button that navigates to
 * `/salvage`. The store-tracked `promptedThisSession` flag prevents
 * the banner from re-firing on every StrictMode remount + Vite
 * hot-reload.
 *
 * IPC failures are intentionally swallowed (logged only) — we'd
 * rather miss the prompt than block startup with a recovery toast
 * the user didn't ask for.
 *
 * **StrictMode double-mount note.** React 19's StrictMode mounts
 * effects twice in dev. Both mounts read `promptedThisSession=false`
 * before either resolves the `await invoke(...)` — so the outer
 * `useEffect` guard is necessary but not sufficient. The flag is
 * re-checked imperatively *after* the IPC resolves (via
 * `useSalvagePromptStore.getState()`) and the mark-then-toast write
 * is atomic against any prior winner. The result is exactly one
 * toast per app launch, even under StrictMode.
 */
function useSalvagePrompt() {
  const navigate = useNavigate();

  useEffect(() => {
    if (useSalvagePromptStore.getState().promptedThisSession) {
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const sessions = await invoke("heron_scan_unfinalized");
        if (cancelled) {
          return;
        }
        // Re-check the store: a sibling effect run (StrictMode) may
        // have already marked the prompt. We claim the toast only if
        // we're the first to flip the flag; subsequent runs see the
        // flag set and bail without firing a duplicate toast.
        if (useSalvagePromptStore.getState().promptedThisSession) {
          return;
        }
        useSalvagePromptStore.getState().markPrompted();
        if (sessions.length === 0) {
          return;
        }
        const count = sessions.length;
        toast(
          `${count} session${count === 1 ? "" : "s"} need recovery`,
          {
            description:
              "Open Salvage to recover or purge cached recordings.",
            action: {
              label: "Open Salvage",
              onClick: () => navigate("/salvage"),
            },
            duration: 10_000,
          },
        );
      } catch (err) {
        // The scan never throws on a missing cache root, so this
        // branch only fires for serious IPC / IO failures. Log and
        // mark prompted so we don't keep retrying on every remount.
        // eslint-disable-next-line no-console
        console.warn("[heron] salvage scan failed:", err);
        useSalvagePromptStore.getState().markPrompted();
      }
    })();
    return () => {
      cancelled = true;
    };
    // `navigate` is stable across renders; intentionally one-shot
    // for the lifetime of the app. The store is read imperatively
    // inside the effect so we don't need to subscribe.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
