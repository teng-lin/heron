/**
 * Onboarding route stub.
 *
 * The five-step Test-button flow lands in a follow-up PR. This stub
 * exists so the route is reachable and so PR-α's typed-invoke surface
 * is exercised by `npm run build`.
 */

import { Link } from "react-router-dom";

export default function Onboarding() {
  return (
    <main className="p-6 space-y-4">
      <h1 className="text-2xl font-semibold">Onboarding</h1>
      <p className="text-muted-foreground">
        Five-step Test-button walkthrough — wired up in a follow-up PR.
      </p>
      <Link to="/" className="underline">
        Back to home
      </Link>
    </main>
  );
}
