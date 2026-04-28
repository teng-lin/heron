import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { Settings as SettingsIcon, Shield } from "lucide-react";

import { HeronWordmark } from "../ui/heron-wordmark";
import { useRecordingStore } from "../../store/recording";
import { ModePill } from "./mode-pill";

/**
 * Top chrome of the main window.
 *
 * On macOS we use `titleBarStyle: "Overlay"` (configured in
 * `tauri.conf.json`), so the system traffic lights sit at the
 * configured `trafficLightPosition` and React renders the rest of the
 * bar below/around them. The 76px left padding reserves the
 * traffic-light gutter; the entire bar (minus interactive controls) is
 * a `data-tauri-drag-region` so the user can drag the window from any
 * empty patch of TitleBar.
 */
export function TitleBar() {
  const navigate = useNavigate();
  const recordingStart = useRecordingStore((s) => s.recordingStart);
  const elapsed = useElapsed(recordingStart);

  return (
    <header
      data-tauri-drag-region
      className="flex h-11 shrink-0 items-center gap-3.5 border-b px-2.5 select-none"
      style={{
        paddingLeft: 76,
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
    >
      <Link
        to="/home"
        className="inline-flex items-center"
        aria-label="Heron home"
      >
        <HeronWordmark size={14} />
      </Link>

      <div className="flex-1" data-tauri-drag-region />

      <ModePill />

      {recordingStart !== null && (
        <span
          className="inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 font-mono text-[10px] uppercase tracking-[0.08em] text-white"
          style={{ background: "var(--color-rec)" }}
        >
          <span
            className="inline-block animate-[pulse-rec_1.4s_ease-in-out_infinite]"
            style={{
              width: 6,
              height: 6,
              borderRadius: "50%",
              background: "white",
            }}
          />
          REC · {elapsed}
        </span>
      )}

      <button
        type="button"
        aria-label="Privacy"
        className="inline-flex h-7 w-7 items-center justify-center rounded text-ink-3 hover:bg-paper-3"
        title="Where your data goes"
        onClick={() => navigate("/privacy")}
      >
        <Shield size={14} aria-hidden="true" />
      </button>
      <button
        type="button"
        aria-label="Settings"
        className="inline-flex h-7 w-7 items-center justify-center rounded text-ink-3 hover:bg-paper-3"
        title="Settings"
        onClick={() => navigate("/settings")}
      >
        <SettingsIcon size={14} aria-hidden="true" />
      </button>
    </header>
  );
}

function useElapsed(recordingStart: number | null): string {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (recordingStart === null) return;
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [recordingStart]);
  if (recordingStart === null) return "00:00";
  const total = Math.max(0, Math.floor((now - recordingStart) / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}
