/**
 * Top-level route switch.
 *
 * - `/` — first-run gate. Loads `Settings` once on mount; if
 *   `settings.onboarded === false` redirects to `/onboarding`,
 *   otherwise jumps to `/review/<latest>` (when the vault has any
 *   notes) or `/home`. While settings load, renders a small Loading
 *   placeholder so a remount under React StrictMode doesn't flash
 *   the wizard for already-onboarded users (PR-ι / phase 71).
 * - `/onboarding` — five-step Test-button walkthrough (PR-ι).
 * - `/home` — dashboard with the `heron_status` smoke test +
 *   "Start recording" entry point (PR-β).
 * - `/recording` — full-window in-progress recording view (PR-β).
 * - `/review/:sessionId` — review UI stub.
 * - `/salvage` — crash-recovery list (PR-η).
 * - `/settings` — settings form stub.
 * - `*` — anything else falls back to `/`. Without this, navigating
 *   to a typo or stale link renders a blank screen rather than
 *   landing back at the first-run gate.
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

import { useEffect, useState } from "react";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";
import { toast } from "sonner";

import ConsentGate from "./components/ConsentGate";
import { useTrayNav } from "./hooks/useTrayNav";
import { invoke } from "./lib/invoke";
import { resolvePostOnboardingDestination } from "./lib/postOnboardingDestination";
import Home from "./pages/Home";
import Onboarding from "./pages/Onboarding";
import Recording from "./pages/Recording";
import Review from "./pages/Review";
import Salvage from "./pages/Salvage";
import Settings from "./pages/Settings";
import { useSalvagePromptStore } from "./store/salvage";
import { useSettingsStore } from "./store/settings";

export default function App() {
  useTrayNav();
  useSalvagePrompt();

  return (
    <>
      <Routes>
        <Route path="/" element={<FirstRunGate />} />
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
 * Loads `Settings` (if not already cached), then redirects:
 *
 * - `settings === null` (still loading) → `<Loading />` placeholder.
 *   Without this branch, the route would render the wizard for a
 *   single frame on every cold start, causing already-onboarded
 *   users to see a flicker.
 * - `settings.onboarded === false` → `/onboarding` (the wizard).
 * - Otherwise, peek at `heron_last_note_session_id`. If the vault
 *   has any notes, jump straight to the latest. If not, land on
 *   `/home` (the recording-controls placeholder).
 *
 * The last-note probe is best-effort: a Tauri / IPC failure falls
 * through to `/home`. Failing closed (the wizard) on a transient
 * read error would re-onboard already-onboarded users, which is
 * worse than landing on the placeholder.
 */
function FirstRunGate() {
  // Subscribe to `settings` so the gate re-renders when the wizard's
  // `markOnboarded` flips the in-memory snapshot. The current
  // implementation routes once (via `setDestination`) and unmounts
  // before the flip can matter, but keeping the subscription keeps
  // the gate honest if a future feature reuses it for a re-onboard
  // flow. The variable is read inside the effect's `ensureLoaded`
  // result rather than directly so we don't accidentally close over
  // a stale snapshot.
  const ensureLoaded = useSettingsStore((state) => state.ensureLoaded);
  const [destination, setDestination] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      // `ensureLoaded` returns the cached snapshot when present, so
      // remounts (StrictMode) skip the IPC round-trip. The snapshot
      // here is the resolved value, NOT the post-effect store
      // contents — `useState` over the store would race a parallel
      // remount; capturing the resolved value sidesteps that.
      const loaded = await ensureLoaded();
      if (cancelled) {
        return;
      }
      if (loaded === null) {
        // Settings load failed. Failing closed (the wizard) is the
        // safe fallback — the worst case is the user runs through
        // the wizard once after a transient settings.json read
        // error, which is annoying but not destructive.
        setDestination("/onboarding");
        return;
      }
      if (!loaded.onboarded) {
        setDestination("/onboarding");
        return;
      }
      // Onboarded — defer to the shared post-onboarding destination
      // helper so this lookup stays in lockstep with the wizard's
      // `Finish setup` landing logic.
      const dest = await resolvePostOnboardingDestination();
      if (cancelled) {
        return;
      }
      setDestination(dest);
    })();
    return () => {
      cancelled = true;
    };
  }, [ensureLoaded]);

  // While the gate's async dance resolves, render a tiny spinner.
  // The settings store may already have a snapshot (StrictMode
  // remount, or a sibling consumer that called `load` first); we
  // still wait for our `ensureLoaded` promise so the destination is
  // computed exactly once.
  if (destination === null) {
    return (
      <div className="flex min-h-screen items-center justify-center text-sm text-muted-foreground">
        Loading…
      </div>
    );
  }
  return <Navigate to={destination} replace />;
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
