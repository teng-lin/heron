/**
 * Home / dashboard route.
 *
 * Acts as a smoke test for the React tree: clicking "Status" calls
 * the `heron_status` Tauri command via the typed `invoke` wrapper and
 * stores the response in Zustand.
 *
 * Phase 64 (PR-β) adds the "Start recording" entry point: it routes
 * through the consent gate (`useConsentStore.requestConsent()`) and,
 * on confirm, seeds `useRecordingStore` with `Date.now()` and
 * navigates to `/recording`. The actual capture pipeline still lives
 * in `heron-cli` and gets wired through Tauri in a later phase — this
 * button only exercises the UI affordance.
 */

import { Link, useNavigate } from "react-router-dom";

import { Button } from "../components/ui/button";
import { useConsentStore } from "../store/consent";
import { useRecordingStore } from "../store/recording";
import { useStatusStore } from "../store/status";

export default function Home() {
  const { status, error, loading, refresh } = useStatusStore();
  const requestConsent = useConsentStore((s) => s.requestConsent);
  const startRecording = useRecordingStore((s) => s.start);
  const navigate = useNavigate();

  const onStart = async () => {
    const decision = await requestConsent();
    if (decision === "confirmed") {
      // Seed the UI store before navigating so the Recording page's
      // first paint shows `00:00:00` rather than reading a stale
      // `recordingStart` from a previous session.
      startRecording();
      navigate("/recording");
    }
  };

  return (
    <main className="p-6 space-y-4">
      <h1 className="text-2xl font-semibold">Home</h1>
      <p className="text-muted-foreground">
        Foundation scaffold. The recording UX lands in PR-γ.
      </p>
      <div className="flex gap-2 flex-wrap">
        <Button onClick={() => void onStart()}>Start recording</Button>
        <Button variant="outline" onClick={() => void refresh()} disabled={loading}>
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
