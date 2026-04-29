/**
 * Sticky audio playback strip for the Review route — PR-ε (phase 67).
 *
 * Resolves the recording for `<sessionId>` via `heron_resolve_recording`
 * and binds the result to a hidden `<audio>` element. Tauri 2's
 * built-in `asset:` protocol (registered via `assetProtocol.enable` in
 * `tauri.conf.json`) is what makes a local-disk path playable from the
 * webview; we route the path through `convertFileSrc()` rather than
 * registering a custom `heron://` scheme so we keep the dependency
 * surface flat — fewer plugins, fewer capabilities, the same security
 * posture (the resolver still does the file-existence + cache-fallback
 * decision in Rust).
 *
 * ## Why a custom strip, not native browser controls?
 *
 * Native `<audio controls>` is fine but the design calls for the
 * timeline to be inline with the rest of the bar; we keep the audio
 * element headless and render our own play / scrubber / time labels
 * so the layout doesn't shift when controls toggle visibility.
 *
 * ## Imperative seek
 *
 * The parent (`Review.tsx`) gets a ref-shaped handle so a click on a
 * transcript row can seek into the audio without the bar's render
 * thrashing on every parent rerender. A controlled-prop `seekTo`
 * would force the parent to manage a debounce — the imperative ref
 * sidesteps that.
 */

import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { Pause, Play } from "lucide-react";

import { Button } from "./ui/button";
import {
  invoke,
  type AssetSource,
} from "../lib/invoke";
import type { DaemonAudioSource } from "../lib/types";

export interface PlaybackBarHandle {
  /** Move the audio playhead to `seconds`. No-op when no recording is
   * loaded (the resolve step failed or hasn't completed). */
  seekTo(seconds: number): void;
}

interface PlaybackBarProps {
  /** Resolved vault root from `useSettingsStore`. The bar derives the
   * `m4a_candidate` (`<vault>/meetings/<sessionId>.m4a`) from this. */
  vaultRoot: string;
  /** Cache root from `heron_default_cache_root`. Threaded into the
   * resolver so the salvage-from-WAV fallback can find the cached
   * `mic.raw` / `tap.raw`. */
  cacheRoot: string;
  /** The session basename (no `.md` suffix). */
  sessionId: string;
  /** Imperative handle so parents can call `seekTo(seconds)` from
   * a transcript-row click. */
  ref?: React.Ref<PlaybackBarHandle>;
}

/**
 * Three-state lifecycle for the resolve step.
 *
 * - `loading`: still asking Rust for the asset path.
 * - `ready`: an `m4a` or salvageable WAV exists; `src` is set.
 * - `partial`: the resolver returned `salvage_from_cache` (m4a not
 *   encoded yet). We surface the empty-state message but keep the
 *   bar mounted so the user knows playback will work eventually.
 *   Today the salvage variant doesn't produce a playable URL — the
 *   actual mixdown wires up in week 13 — so the bar is informational.
 * - `missing`: no recording on disk at all (also covers the partial-
 *   cache `Err` case where one raw file exists but not the other).
 */
type Resolved =
  | { kind: "loading" }
  | { kind: "ready"; src: string; sourceLabel: string }
  | { kind: "partial"; sourceLabel: string }
  | { kind: "missing"; message: string };

/**
 * Format a non-negative duration as `M:SS` or `H:MM:SS`.
 *
 * Pure presentational helper. Returns `--:--` for `NaN` /
 * `Infinity` so a freshly-mounted `<audio>` element (which reports
 * `NaN` for `duration` until metadata arrives) renders cleanly
 * instead of `NaN:NaN`.
 */
function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "--:--";
  const total = Math.floor(seconds);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}:${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

/**
 * Build the m4a candidate path the asset-protocol resolver checks
 * first. v1 lays out vaults as `<vault>/meetings/<id>.m4a`; the
 * resolver falls back to the salvage-from-cache path if this file
 * doesn't exist or is empty.
 */
function m4aCandidatePath(vaultRoot: string, sessionId: string): string {
  // Use forward slashes; macOS/Linux accept them and the Tauri side
  // canonicalizes either way. We don't import `path-browserify` for
  // one join — that would be over-engineering.
  return `${vaultRoot}/meetings/${sessionId}.m4a`;
}

/**
 * Shared chrome for the bar in non-ready states. The fixed `h-12`
 * keeps the layout stable so the page doesn't jump when the resolve
 * step transitions between loading → ready → missing.
 */
const BAR_BASE =
  "h-12 px-4 bg-muted/40 border-t border-border flex items-center text-xs text-muted-foreground";

export function PlaybackBar({
  vaultRoot,
  cacheRoot,
  sessionId,
  ref,
}: PlaybackBarProps) {
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const [resolved, setResolved] = useState<Resolved>({ kind: "loading" });
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState<number>(NaN);

  // Resolve the asset whenever the session/vault/cache changes.
  useEffect(() => {
    let cancelled = false;
    setResolved({ kind: "loading" });
    setPlaying(false);
    setCurrentTime(0);
    setDuration(NaN);

    async function resolveLocal() {
      const source: AssetSource = await invoke("heron_resolve_recording", {
        sessionId,
        m4aCandidate: m4aCandidatePath(vaultRoot, sessionId),
        cacheRoot,
      });
      if (source.kind === "m4a") {
        return {
          kind: "ready" as const,
          src: convertFileSrc(source.path),
          sourceLabel: "Archival m4a",
        };
      }
      return {
        kind: "partial" as const,
        sourceLabel: "Audio not yet encoded",
      };
    }

    async function resolveDaemon() {
      const daemon = await invoke("heron_meeting_audio", {
        meetingId: sessionId,
      });
      if (daemon.kind !== "ok") {
        throw new Error(daemon.detail);
      }
      const source: DaemonAudioSource = daemon.data;
      return {
        kind: "ready" as const,
        src: convertFileSrc(source.path),
        sourceLabel: "Daemon audio",
      };
    }

    async function resolve() {
      try {
        const local = await resolveLocal();
        if (local.kind === "ready") return local;
        try {
          return await resolveDaemon();
        } catch {
          return local;
        }
      } catch (localErr) {
        try {
          return await resolveDaemon();
        } catch {
          throw localErr;
        }
      }
    }

    resolve()
      .then((next) => {
        if (!cancelled) setResolved(next);
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          const message = err instanceof Error ? err.message : String(err);
          setResolved({ kind: "missing", message });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [vaultRoot, cacheRoot, sessionId]);

  const togglePlay = useCallback(() => {
    const audio = audioRef.current;
    if (!audio) return;
    if (audio.paused) {
      void audio.play().catch(() => {
        // Autoplay-policy / decode failures surface here. The bar
        // stays in the `playing=false` state because we never set
        // it to `true` — the catch is a safety net for the silent
        // case where the promise rejects.
        setPlaying(false);
      });
    } else {
      audio.pause();
    }
  }, []);

  const onScrub = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const audio = audioRef.current;
      if (!audio) return;
      const next = Number(e.target.value);
      if (Number.isFinite(next)) {
        audio.currentTime = next;
        setCurrentTime(next);
      }
    },
    [],
  );

  // Imperative handle so the transcript row can seek without the bar
  // rerendering on every parent state change.
  useImperativeHandle(
    ref,
    (): PlaybackBarHandle => ({
      seekTo: (seconds: number) => {
        const audio = audioRef.current;
        if (!audio) return;
        if (!Number.isFinite(seconds) || seconds < 0) return;
        // Clamp to known duration when available — a click on a
        // truncated transcript shouldn't drive `currentTime` past
        // the end of the file.
        const max =
          Number.isFinite(audio.duration) && audio.duration > 0
            ? audio.duration
            : seconds;
        const clamped = Math.min(seconds, max);
        audio.currentTime = clamped;
        setCurrentTime(clamped);
        // Convenience: start playback when the user explicitly seeks
        // from the transcript. Mirrors the spirit of the YouTube /
        // Audacity pattern — you clicked, you wanted to hear.
        if (audio.paused) {
          void audio.play().catch(() => {
            /* see togglePlay */
          });
        }
      },
    }),
    [],
  );

  // The empty/loading states still render the strip so the layout
  // doesn't jump when the resolve completes.
  if (resolved.kind === "loading") {
    return <div className={BAR_BASE}>Loading audio…</div>;
  }
  if (resolved.kind === "missing") {
    return (
      <div className={BAR_BASE} title={resolved.message}>
        No recording on disk for this session.
      </div>
    );
  }
  if (resolved.kind === "partial") {
    return (
      <div className={`${BAR_BASE} justify-between`}>
        <span>{resolved.sourceLabel}</span>
        <span>The recording will be playable once the m4a is encoded.</span>
      </div>
    );
  }

  return (
    <div className="px-4 py-2 bg-muted/40 border-t border-border flex items-center gap-3">
      {/* The audio element drives state via its events; we render the
          UI ourselves so the strip layout stays stable. `preload="metadata"`
          gets us a duration + duration-changed event without downloading
          the whole file up front. */}
      <audio
        ref={audioRef}
        src={resolved.src}
        preload="metadata"
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onTimeUpdate={(e) => setCurrentTime(e.currentTarget.currentTime)}
        onLoadedMetadata={(e) => setDuration(e.currentTarget.duration)}
        onDurationChange={(e) => setDuration(e.currentTarget.duration)}
        onEnded={() => setPlaying(false)}
        // No `controls` — we render our own.
      />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        onClick={togglePlay}
        aria-label={playing ? "Pause" : "Play"}
        title={playing ? "Pause" : "Play"}
      >
        {playing ? (
          <Pause className="h-4 w-4" aria-hidden="true" />
        ) : (
          <Play className="h-4 w-4" aria-hidden="true" />
        )}
      </Button>
      <span className="text-xs font-mono tabular-nums text-muted-foreground w-12 text-right">
        {formatDuration(currentTime)}
      </span>
      <input
        type="range"
        min={0}
        // `step="any"` so the slider lets a click land on any second
        // rather than rounding to integer steps; the audio element
        // accepts fractional `currentTime` too.
        step="any"
        max={Number.isFinite(duration) && duration > 0 ? duration : 0}
        value={Number.isFinite(currentTime) ? currentTime : 0}
        onChange={onScrub}
        // Tailwind's `accent-primary` matches the brand color on the
        // native range thumb; the rest of the slider styling falls
        // back to OS defaults so the bar feels native.
        className="flex-1 accent-primary"
        aria-label="Audio scrub"
        disabled={!Number.isFinite(duration) || duration <= 0}
      />
      <span className="text-xs font-mono tabular-nums text-muted-foreground w-12">
        {formatDuration(duration)}
      </span>
      <span
        className="text-xs text-muted-foreground"
        title={resolved.sourceLabel}
      >
        {resolved.sourceLabel}
      </span>
    </div>
  );
}
