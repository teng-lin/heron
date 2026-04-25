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
 */

import { Navigate, Route, Routes } from "react-router-dom";

import ConsentGate from "./components/ConsentGate";
import { useTrayNav } from "./hooks/useTrayNav";
import Home from "./pages/Home";
import Onboarding from "./pages/Onboarding";
import Recording from "./pages/Recording";
import Review from "./pages/Review";
import Settings from "./pages/Settings";

export default function App() {
  useTrayNav();

  return (
    <>
      <Routes>
        <Route path="/" element={<Navigate to="/onboarding" replace />} />
        <Route path="/home" element={<Home />} />
        <Route path="/onboarding" element={<Onboarding />} />
        <Route path="/recording" element={<Recording />} />
        <Route path="/review/:sessionId" element={<Review />} />
        <Route path="/settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
      <ConsentGate />
    </>
  );
}
