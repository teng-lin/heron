/**
 * Athena stub page — `/athena`.
 *
 * UI shell for the planned Athena mode (sidebar suggestions during a
 * live call). The data here is hardcoded mock from `lib/mocks/athena-feed.ts`;
 * Athena's classifier + suggestion engine ships at week ~11 per
 * `docs/heron-implementation.md`. The "in development" banner makes
 * the placeholder explicit so users don't think the feature is
 * already wired.
 */

import { Lightbulb, MessageSquare, Flag } from "lucide-react";

import {
  ATHENA_FEED,
  type AthenaSuggestion,
  type AthenaSuggestionKind,
} from "../lib/mocks/athena-feed";

export default function Athena() {
  return (
    <main className="mx-auto w-full max-w-4xl px-8 py-10">
      <ComingSoonBanner
        timeline="week ~11"
        body="Athena will surface suggestions live during a call: facts pulled from your vault, suggested replies, and triggers for topics you've flagged in your briefing."
      />

      <header className="mt-8 mb-6">
        <p
          className="font-mono text-xs uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          Athena · live counsel
        </p>
        <h1
          className="mt-1 font-serif text-[28px] leading-tight"
          style={{ color: "var(--color-ink)", letterSpacing: "-0.02em" }}
        >
          Sample suggestion feed
        </h1>
        <p
          className="mt-2 max-w-prose text-sm"
          style={{ color: "var(--color-ink-2)" }}
        >
          This is a frozen UI snapshot — the rows below are static
          fixtures, not real outputs.
        </p>
      </header>

      <ul className="space-y-3">
        {ATHENA_FEED.map((s, i) => (
          <Row key={`${s.at}-${i}`} suggestion={s} />
        ))}
      </ul>
    </main>
  );
}

const KIND_TONE: Record<
  AthenaSuggestionKind,
  { label: string; color: string; Icon: typeof Lightbulb }
> = {
  fact: { label: "Fact", color: "var(--color-ok)", Icon: Lightbulb },
  reply: {
    label: "Reply",
    color: "var(--color-accent)",
    Icon: MessageSquare,
  },
  flag: { label: "Flag", color: "var(--color-warn)", Icon: Flag },
};

function Row({ suggestion }: { suggestion: AthenaSuggestion }) {
  const tone = KIND_TONE[suggestion.kind];
  const Icon = tone.Icon;
  return (
    <li
      className="rounded border p-4"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <div className="mb-1.5 flex items-center gap-2">
        <span
          className="inline-flex items-center gap-1 font-mono text-[10px] uppercase tracking-[0.12em]"
          style={{ color: tone.color }}
        >
          <Icon size={12} />
          {tone.label}
        </span>
        <span
          className="font-mono text-[10px]"
          style={{ color: "var(--color-ink-3)" }}
        >
          {suggestion.at}
        </span>
      </div>
      <p
        className="font-serif text-base"
        style={{ color: "var(--color-ink)" }}
      >
        {suggestion.title}
      </p>
      <p
        className="mt-1 text-sm leading-relaxed"
        style={{ color: "var(--color-ink-2)" }}
      >
        {suggestion.body}
      </p>
      {suggestion.source && (
        <p
          className="mt-2 font-mono text-[10px]"
          style={{ color: "var(--color-ink-4)" }}
        >
          {suggestion.source}
        </p>
      )}
    </li>
  );
}

function ComingSoonBanner({
  timeline,
  body,
}: {
  timeline: string;
  body: string;
}) {
  return (
    <div
      className="rounded border px-4 py-3"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-warn)",
      }}
    >
      <p
        className="font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-warn)" }}
      >
        In development · {timeline}
      </p>
      <p
        className="mt-1 text-sm leading-relaxed"
        style={{ color: "var(--color-ink-2)" }}
      >
        {body}
      </p>
    </div>
  );
}
