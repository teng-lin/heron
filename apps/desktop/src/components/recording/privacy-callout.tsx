/**
 * Privacy reminder card on the Recording right rail. Reinforces the
 * local-first posture during a live capture: nothing leaves the
 * machine until summarization is explicitly triggered.
 *
 * Pure UI; clicking the body navigates to /privacy where the full
 * flow diagram + per-mode policies live.
 */

import { Shield } from "lucide-react";
import { Link } from "react-router-dom";

export function PrivacyCallout() {
  return (
    // Use Tailwind utilities for `background` so the `:hover` rule
    // can override at the class level — an inline `style.background`
    // would beat the hover utility on specificity (1000 vs 11) and
    // the rollover would visually do nothing.
    <Link
      to="/privacy"
      className="block rounded-md border bg-paper p-3.5 transition-colors hover:bg-paper-3 no-underline"
      style={{
        borderColor: "var(--color-rule)",
        color: "var(--color-ink-2)",
      }}
    >
      <div className="mb-1.5 flex items-center gap-2">
        <Shield size={13} aria-hidden="true" />
        <span
          className="text-[12px] font-medium"
          style={{ color: "var(--color-ink)" }}
        >
          Audio stays here
        </span>
      </div>
      <p
        className="m-0 font-mono text-[10.5px] leading-[1.5]"
        style={{ color: "var(--color-ink-3)" }}
      >
        Nothing leaves this Mac while you&rsquo;re recording. The transcript
        only goes to your LLM provider when you trigger summarization.
      </p>
    </Link>
  );
}
