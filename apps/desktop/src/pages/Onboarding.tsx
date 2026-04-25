/**
 * Onboarding route stub.
 *
 * The five-step Test-button flow lands in a follow-up PR. This stub
 * exists so the route is reachable and so PR-α's typed-invoke surface
 * is exercised by `bun run build`.
 */

import { Link } from "react-router-dom";

export default function Onboarding() {
  return (
    <main className="p-6 space-y-4">
      <h1 className="text-2xl font-semibold">Onboarding</h1>
      <p className="text-muted-foreground">
        Five-step Test-button walkthrough — wired up in a follow-up PR.
      </p>
      {/* `/` redirects to this same route, so back-links go to `/home`
          (the dashboard) until first-run detection lands in PR-δ. */}
      <Link to="/home" className="underline">
        Back to home
      </Link>
    </main>
  );
}
