import { AlertTriangle, RotateCw } from "lucide-react";

import { useMeetingsStore } from "../store/meetings";

/**
 * Sticky banner shown when the daemon is unreachable. Rendered by
 * pages that depend on daemon-driven data (Home, Recording, Review);
 * Settings and Salvage are deliberately excluded since they keep
 * working without the daemon. Retry re-runs the meetings load — if
 * other pages add their own retryable calls later, swap the onRetry
 * prop or hoist the retry registry into a dedicated store.
 */
export function DaemonDownBanner() {
  const daemonDown = useMeetingsStore((s) => s.daemonDown);
  const error = useMeetingsStore((s) => s.error);
  const load = useMeetingsStore((s) => s.load);

  if (!daemonDown) return null;

  return (
    <div
      className="flex items-center gap-3 border-b px-4 py-2"
      style={{
        background: "var(--color-paper-3)",
        borderColor: "var(--color-rule)",
        color: "var(--color-ink-2)",
      }}
      role="status"
    >
      <AlertTriangle
        size={14}
        style={{ color: "var(--color-warn)", flexShrink: 0 }}
      />
      <div className="flex-1 text-xs">
        Can't reach the heron daemon — settings and salvage still work.
        {error && (
          <span
            className="ml-2 font-mono text-[10px]"
            style={{ color: "var(--color-ink-3)" }}
          >
            ({error})
          </span>
        )}
      </div>
      <button
        type="button"
        onClick={() => void load()}
        className="inline-flex items-center gap-1 rounded border px-2 py-1 text-xs"
        style={{
          background: "var(--color-paper)",
          borderColor: "var(--color-rule-2)",
          color: "var(--color-ink)",
        }}
      >
        <RotateCw size={12} />
        Retry
      </button>
    </div>
  );
}
