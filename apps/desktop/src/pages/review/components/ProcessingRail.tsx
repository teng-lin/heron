import type { MeetingProcessing } from "../../../lib/types";
import { TOKEN_COUNT_FORMATTER, formatProcessingCost } from "../utils/format";

/**
 * Tier 0 #2 right-rail. Renders `Meeting.processing` (model + token
 * counts + cost). Omitted by the parent when `processing` is
 * `undefined` — pre-summarize meetings shouldn't render `—`
 * placeholders. `Transcribed by` is intentionally absent because
 * `Frontmatter.stt_model` does not exist yet (separate backend
 * workstream, not in this PR's scope).
 */
export function ProcessingRail({
  processing,
}: {
  processing: MeetingProcessing;
}) {
  return (
    <aside
      aria-label="Processing"
      className="rounded border p-4"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
        color: "var(--color-ink-2)",
      }}
    >
      <h2
        className="mb-3 font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-ink-3)" }}
      >
        Processing
      </h2>
      <dl className="grid grid-cols-[8rem_1fr] gap-y-2 text-xs">
        <dt style={{ color: "var(--color-ink-3)" }}>Summarized by</dt>
        <dd className="font-mono break-all" style={{ color: "var(--color-ink)" }}>
          {processing.model}
        </dd>
        <dt style={{ color: "var(--color-ink-3)" }}>Tokens in</dt>
        <dd className="font-mono" style={{ color: "var(--color-ink)" }}>
          {TOKEN_COUNT_FORMATTER.format(processing.tokens_in)}
        </dd>
        <dt style={{ color: "var(--color-ink-3)" }}>Tokens out</dt>
        <dd className="font-mono" style={{ color: "var(--color-ink)" }}>
          {TOKEN_COUNT_FORMATTER.format(processing.tokens_out)}
        </dd>
        <dt style={{ color: "var(--color-ink-3)" }}>Cost</dt>
        <dd className="font-mono" style={{ color: "var(--color-ink)" }}>
          {formatProcessingCost(processing.summary_usd)}
        </dd>
      </dl>
    </aside>
  );
}
