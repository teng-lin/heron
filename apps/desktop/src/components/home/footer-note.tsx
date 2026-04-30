/**
 * Privacy reminder under the meetings table — a small editorial coda
 * that reinforces the local-first posture before the persistent Ask
 * bar at the bottom of Home. The "Where your data goes →" link
 * navigates to /privacy where the full flow diagram + per-mode
 * policies live.
 */

import { Shield } from "lucide-react";
import { Link } from "react-router-dom";

export function HomeFooterNote() {
  return (
    <footer
      className="flex items-center gap-3.5 px-14 pt-5 pb-24"
      style={{ color: "var(--color-ink-3)" }}
    >
      <Shield size={13} aria-hidden="true" />
      <p className="m-0 font-mono text-[11px] leading-snug">
        Audio stays local. Transcripts go only to the LLM provider whose key
        you supplied.{" "}
        <Link
          to="/privacy"
          className="border-b transition-colors hover:border-current"
          style={{
            color: "var(--color-ink)",
            borderColor: "var(--color-ink-4)",
            textDecoration: "none",
            paddingBottom: 1,
          }}
        >
          Where your data goes →
        </Link>
      </p>
    </footer>
  );
}
