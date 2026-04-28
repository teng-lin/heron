/**
 * Privacy / data-flow page. Mode-aware: shows what stays on-device
 * vs. what leaves for the user's currently-selected companion mode.
 *
 * Source of truth for the per-mode rows is `docs/heron-vision.md`
 * §"Where your data goes" — keep this page synchronized when that
 * doc changes. The copy is intentionally hardcoded here rather than
 * generated from a JSON config because the privacy story is part
 * of the product's promise; it should not silently flip due to a
 * config edit.
 */

import { useActiveMode } from "../store/mode";
import type { ActiveMode } from "../lib/invoke";

interface FlowRow {
  label: string;
  detail: string;
}

interface ModePolicy {
  stays: FlowRow[];
  leaves: FlowRow[];
  notes?: string;
}

const POLICY: Record<ActiveMode, ModePolicy> = {
  clio: {
    stays: [
      { label: "Audio capture", detail: "macOS Core Audio process tap — never written off-device." },
      { label: "Transcript", detail: "WhisperKit on-device. The model lives in your ~/Library cache." },
      { label: "Notes", detail: "Markdown in your Obsidian vault. heron never reads them back to a server." },
    ],
    leaves: [
      { label: "Summary LLM call", detail: "When you have an Anthropic / OpenAI key set, the transcript is sent for a one-shot summary. Disable in Settings → General to keep summaries local." },
    ],
    notes: "Clio mode is the strictest privacy posture heron offers. The summary LLM is the only optional outbound call; everything else stays on this Mac.",
  },
  athena: {
    stays: [
      { label: "Audio capture", detail: "Same as Clio — local Core Audio tap, never persisted off-device." },
      { label: "Transcript", detail: "On-device WhisperKit." },
      { label: "Vault context", detail: "Athena reads notes from your vault to surface relevant suggestions; the reads happen locally." },
    ],
    leaves: [
      { label: "LLM suggestions", detail: "The classifier sends rolling transcript windows + matched vault snippets to the LLM provider you configure. This is the cost of Athena — the suggestions are LLM-generated." },
    ],
    notes: "Athena is in development. The privacy posture above describes the planned default; flip to Clio mode for the strictest stance until it ships.",
  },
  pollux: {
    stays: [
      { label: "Audio capture", detail: "Same as Clio. Pollux ALSO captures your voice for the clone, kept locally encrypted at rest." },
      { label: "Voice clone", detail: "If you choose a local TTS engine, your cloned voice never leaves the device." },
    ],
    leaves: [
      { label: "Voice clone provider call", detail: "If you choose ElevenLabs / OpenAI for cloning, samples + synthesized audio cross the network. Provider-specific retention applies." },
      { label: "LLM hand-off classifier", detail: "Same call shape as Athena." },
    ],
    notes: "Pollux is in development. The voice-clone consent flow is intentionally separate from the meeting-disclosure consent (ConsentGate) — see docs/heron-implementation.md for the BIPA / GDPR posture.",
  },
};

export default function Privacy() {
  const mode = useActiveMode();
  const policy = POLICY[mode];

  return (
    <main className="mx-auto w-full max-w-4xl px-8 py-10">
      <header className="mb-8">
        <p
          className="font-mono text-xs uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          Mode · {mode}
        </p>
        <h1
          className="mt-1 font-serif text-[32px] leading-tight"
          style={{ color: "var(--color-ink)", letterSpacing: "-0.02em" }}
        >
          Where your data goes
        </h1>
        <p
          className="mt-2 max-w-prose text-sm"
          style={{ color: "var(--color-ink-2)" }}
        >
          Switch the mode pill in the title bar to see how each mode's
          privacy posture differs.
        </p>
      </header>

      <div className="grid gap-6 md:grid-cols-2">
        <Column
          tone="ok"
          eyebrow="Stays on-device"
          rows={policy.stays}
        />
        <Column
          tone="warn"
          eyebrow="Leaves the device"
          rows={policy.leaves}
        />
      </div>

      {policy.notes && (
        <div
          className="mt-8 rounded border px-4 py-3 text-sm"
          style={{
            background: "var(--color-paper-2)",
            borderColor: "var(--color-rule)",
            color: "var(--color-ink-2)",
          }}
        >
          {policy.notes}
        </div>
      )}
    </main>
  );
}

function Column({
  tone,
  eyebrow,
  rows,
}: {
  tone: "ok" | "warn";
  eyebrow: string;
  rows: FlowRow[];
}) {
  const accent =
    tone === "ok" ? "var(--color-ok)" : "var(--color-warn)";
  return (
    <section
      className="rounded border p-5"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <p
        className="mb-4 font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: accent }}
      >
        {eyebrow}
      </p>
      {rows.length === 0 ? (
        <p
          className="text-sm italic"
          style={{ color: "var(--color-ink-3)" }}
        >
          Nothing in this category.
        </p>
      ) : (
        <ul className="space-y-3">
          {rows.map((row) => (
            <li key={row.label}>
              <p
                className="font-serif text-base"
                style={{ color: "var(--color-ink)" }}
              >
                {row.label}
              </p>
              <p
                className="mt-1 text-sm leading-relaxed"
                style={{ color: "var(--color-ink-2)" }}
              >
                {row.detail}
              </p>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
