/**
 * Top-level route switch.
 *
 * - `/` — placeholder redirect to `/onboarding`. Real first-run
 *   detection (read `Settings`, branch on `vault_root` being empty)
 *   lands in PR-δ.
 * - `/onboarding` — five-step walkthrough stub (PR-α).
 * - `/home` — dashboard with the `heron_status` smoke test.
 * - `/review/:sessionId` — review UI stub.
 * - `/settings` — settings form stub.
 * - `*` — anything else falls back to `/`. Without this, navigating
 *   to a typo or stale link renders a blank screen rather than
 *   landing back at the onboarding redirect.
 */

import { Navigate, Route, Routes } from "react-router-dom";

import Home from "./pages/Home";
import Onboarding from "./pages/Onboarding";
import Review from "./pages/Review";
import Settings from "./pages/Settings";

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/onboarding" replace />} />
      <Route path="/home" element={<Home />} />
      <Route path="/onboarding" element={<Onboarding />} />
      <Route path="/review/:sessionId" element={<Review />} />
      <Route path="/settings" element={<Settings />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
