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
 * navigates to `/recording`.
 *
 * Phase 73 (PR-λ) adds a pre-flight disk-space gate ahead of the
 * consent modal: `heron_check_disk_for_recording` is called first;
 * if the cache volume is below the user's `min_free_disk_mib`
 * threshold, a warning modal explains the shortfall and offers
 * "Continue anyway" / "Cancel" / "Open vault folder". Only on
 * confirm/override does the consent gate run.
 */

import { useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { Link, useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { Button } from "../components/ui/button";
import {
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../components/ui/dialog";
import { invoke } from "../lib/invoke";
import { useConsentStore } from "../store/consent";
import { useRecordingStore } from "../store/recording";
import { useSettingsStore } from "../store/settings";
import { useStatusStore } from "../store/status";

interface DiskWarning {
  freeMib: number;
  thresholdMib: number;
}

export default function Home() {
  const { status, error, loading, refresh } = useStatusStore();
  const requestConsent = useConsentStore((s) => s.requestConsent);
  const startRecording = useRecordingStore((s) => s.start);
  const ensureLoaded = useSettingsStore((s) => s.ensureLoaded);
  const navigate = useNavigate();
  const [diskWarning, setDiskWarning] = useState<DiskWarning | null>(null);

  async function proceedToConsent() {
    const decision = await requestConsent();
    if (decision === "confirmed") {
      // Seed the UI store before navigating so the Recording page's
      // first paint shows `00:00:00` rather than reading a stale
      // `recordingStart` from a previous session.
      startRecording();
      navigate("/recording");
    }
  }

  async function onStart() {
    // PR-λ pre-flight disk-space gate. The check runs ahead of consent
    // so a user who's out of room never sees the consent modal — the
    // disk warning is the higher-priority signal. On `Ok` we proceed
    // straight to consent.
    try {
      const settingsPath = await invoke("heron_default_settings_path");
      const outcome = await invoke("heron_check_disk_for_recording", {
        settingsPath,
      });
      if (outcome.kind === "below_threshold") {
        setDiskWarning({
          freeMib: outcome.free_mib,
          thresholdMib: outcome.threshold_mib,
        });
        return;
      }
    } catch (err) {
      // The pre-flight check is non-blocking on IPC failure: we'd
      // rather let the user start recording than fail the start
      // because settings.json couldn't be read. Log + fall through.
      // eslint-disable-next-line no-console
      console.warn("[heron] disk pre-flight failed at start:", err);
    }
    await proceedToConsent();
  }

  async function continueAnyway() {
    setDiskWarning(null);
    await proceedToConsent();
  }

  async function openVaultFolder() {
    // The brief calls for "@tauri-apps/plugin-shell if available, else
    // just prints the path". `plugin-shell` isn't a current dep; fall
    // back to a toast that surfaces the path so the user can navigate
    // to it manually. Keeps the diff small.
    try {
      const settings = await ensureLoaded();
      const target = settings?.vault_root ?? "";
      if (target) {
        toast.info(`Vault path: ${target}`, {
          description: "Open this folder in Finder and free up space.",
          duration: 12_000,
        });
      } else {
        toast.info("No vault path set yet — pick one in Settings → General.");
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not resolve vault path: ${message}`);
    }
  }

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
      <Dialog.Root
        open={diskWarning !== null}
        onOpenChange={(next) => {
          if (!next) setDiskWarning(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Low disk space</DialogTitle>
          </DialogHeader>
          {diskWarning && (
            <p className="text-sm">
              Only <strong>{diskWarning.freeMib} MiB</strong> free, threshold
              is <strong>{diskWarning.thresholdMib} MiB</strong>. Free up disk
              before recording.
            </p>
          )}
          <div className="flex flex-wrap justify-end gap-2 mt-2">
            <Button variant="ghost" onClick={() => setDiskWarning(null)}>
              Cancel
            </Button>
            <Button variant="outline" onClick={() => void openVaultFolder()}>
              Open vault folder
            </Button>
            <Button onClick={() => void continueAnyway()}>
              Continue anyway
            </Button>
          </div>
        </DialogContent>
      </Dialog.Root>
    </main>
  );
}
