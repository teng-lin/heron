/**
 * Home / dashboard route.
 *
 * Acts as a smoke test for the React tree: clicking "Status" calls
 * the `heron_status` Tauri command via the typed `invoke` wrapper and
 * stores the response in Zustand. PR-γ replaces this with the real
 * recording controls; today it exists to prove the plumbing.
 */

import { Link } from "react-router-dom";

import { Button } from "../components/ui/button";
import { useStatusStore } from "../store/status";

export default function Home() {
  const { status, error, loading, refresh } = useStatusStore();

  return (
    <main className="p-6 space-y-4">
      <h1 className="text-2xl font-semibold">Home</h1>
      <p className="text-muted-foreground">
        Foundation scaffold. The recording UX lands in PR-γ.
      </p>
      <div className="flex gap-2">
        <Button onClick={() => void refresh()} disabled={loading}>
          {loading ? "Refreshing…" : "Status"}
        </Button>
        <Button variant="outline" asChild>
          <Link to="/onboarding">Onboarding</Link>
        </Button>
        <Button variant="outline" asChild>
          <Link to="/settings">Settings</Link>
        </Button>
      </div>
      {error && (
        <pre className="text-destructive whitespace-pre-wrap">{`error: ${error}`}</pre>
      )}
      {status && (
        <pre className="bg-muted text-foreground p-3 rounded-md overflow-auto">
          {JSON.stringify(status, null, 2)}
        </pre>
      )}
    </main>
  );
}
