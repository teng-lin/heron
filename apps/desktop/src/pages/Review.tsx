/**
 * Review route stub for `/review/:sessionId`.
 *
 * The TipTap-driven transcript + diagnostics tabs land in PR-γ. The
 * stub displays the captured `:sessionId` so the route param is
 * visibly threaded through.
 */

import { Link, useParams } from "react-router-dom";

export default function Review() {
  const { sessionId } = useParams<{ sessionId: string }>();
  return (
    <main className="p-6 space-y-4">
      <h1 className="text-2xl font-semibold">Review</h1>
      <p className="text-muted-foreground">
        Session: <code>{sessionId ?? "(none)"}</code>
      </p>
      <p className="text-muted-foreground">
        Transcript + diagnostics tabs ship in PR-γ.
      </p>
      <Link to="/" className="underline">
        Back to home
      </Link>
    </main>
  );
}
