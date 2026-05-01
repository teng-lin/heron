# Observability

This document covers the metrics surface introduced in issue #223.
Logging conventions (the `tracing` JSON sink + per-session summary
record) live in [`docs/archives/observability.md`](archives/observability.md);
metrics are the new layer that composes alongside logs, not a
replacement.

## Library choice

Heron uses the [`metrics`](https://crates.io/crates/metrics) crate as
the call-site facade and
[`metrics-exporter-prometheus`](https://crates.io/crates/metrics-exporter-prometheus)
as the recorder.

Three options were considered:

| Option | Pros | Cons | Verdict |
| --- | --- | --- | --- |
| `metrics` crate + Prometheus exporter | Facade decouples call sites from exporter; rich ecosystem; idiomatic; easy to swap recorder later (StatsD, OTLP) without rewriting call sites. | One extra crate vs. hand-rolled. | **Chosen.** |
| Hand-rolled counters over `tracing` events | Zero new deps; reuses the existing JSON sink. | Histograms hard; aggregation lives downstream; every consumer parses JSON. Reinvents what `metrics` crate already solved. | Rejected. |
| `opentelemetry` crate | OTLP is the long-term standard. | Heavy dep tree; v1 ships local-only and the Prometheus exposition endpoint is sufficient; OTLP can be plugged in later behind the same `metrics::counter!` call sites. | Deferred. The facade choice keeps the door open. |

The facade matters: every instrumentation site uses
`metrics::counter!`, `metrics::histogram!`, `metrics::gauge!`. Today
those macros dispatch into the Prometheus recorder; a future
StatsD or OTLP swap is one wiring change in `herond/src/main.rs`,
not a workspace-wide search-and-replace.

## Metric primitives

| Kind | When to use | Example |
| --- | --- | --- |
| Counter | Monotonic event count. | `capture_started_total`, `llm_calls_total`, `vault_write_failures_total`. |
| Histogram | Latency or size distribution. | `llm_call_duration_seconds`, `vault_note_size_bytes`. |
| Gauge | Point-in-time state. | `salvage_candidates_pending`, `replay_cache_depth`, `active_captures_count`. |

Use the `metrics` crate macros directly:

```rust
use heron_metrics::{SMOKE_CAPTURE_STARTED_TOTAL, redacted};

metrics::counter!(
    SMOKE_CAPTURE_STARTED_TOTAL,
    "platform" => redacted!("zoom").into_inner(),
)
.increment(1);
```

## Naming convention

Prometheus-style snake_case with the unit in the suffix.

- **Counters end in `_total`.** `capture_started_total`,
  `llm_calls_total`, `vault_write_failures_total`. Without the
  suffix, Prometheus client libraries can't tell counter from
  gauge in dashboards.
- **Histograms end in their unit.** `_seconds` for latency,
  `_milliseconds` if a sub-second range is genuinely useful (rare;
  prefer seconds), `_bytes` for size.
- **Gauges end in their unit or use `_count` / `_pending` /
  `_ratio`.** `replay_cache_depth_count`, `salvage_candidates_pending`,
  `aec_residual_ratio`.
- **Build / version metrics end in `_info`.** Constant-1 metrics
  whose labels carry the version string.

The convention is enforced in code by
[`heron_metrics::validate_metric_name`] and the [`metric_name!`]
macro: a literal that doesn't match panics the first time the
call site is reached. Sub-issues #224 / #225 / #226 should
declare every metric name as a `static` or `let` binding via the
macro:

```rust
static LLM_CALL_DURATION_SECONDS: &str = heron_metrics::metric_name!(
    "llm_call_duration_seconds"
);
```

Drifted names panic at first call. Add a unit test that exercises
the call site once so CI surfaces the panic rather than
production.

## Privacy posture (CRITICAL)

**Default-deny.** Metric labels never carry user-content-derived
strings. Anything below leaks user content into the time series
database, into a future Prometheus scrape's HTTP response, into a
debug `curl /v1/__metrics`, and into any diagnostics bundle:

| Don't put in a label | Why |
| --- | --- |
| Transcript text or excerpts | Privacy; cardinality explosion. |
| Participant names / attendee emails | Privacy. |
| Meeting titles | Privacy; cardinality. |
| API keys / bearer tokens / signing secrets | Credential leak. |
| Raw filesystem paths containing the user's home | Privacy (`alice` in `/Users/alice/...`). |
| Note bodies | Privacy. |
| Raw `meeting_id` / `event_id` UUIDs | Cardinality (hundreds per user-week). Hash with `RedactedLabel::hashed` if grouping by session is genuinely needed; otherwise drop. |

This is enforced **at the type level** by
[`heron_metrics::RedactedLabel`]. A label value is only
constructable through:

1. The `redacted!("literal")` macro — accepts only string literals,
   validates charset and length at construction.
2. `RedactedLabel::from_static(s: &'static str)` — the `'static`
   bound is what blocks the `format!()` bypass:
   `format!("meeting-{id}")` produces a `String`, not a `&'static str`,
   and won't compile.
3. `RedactedLabel::hashed(input: &str)` — produces a stable
   16-hex-char digest. Use only when the cardinality is bounded
   AND grouping by an opaque correlation key is justified.

There is **no** `From<String>` for `RedactedLabel`, no
`From<&str>` for non-static references, no `Display`-via-format
constructor. A reviewer seeing any of:

```rust
redacted!("meeting-{id}")              // FAILS to compile (not a literal)
RedactedLabel::from_static(&id)        // FAILS to compile (not 'static)
RedactedLabel::from_static(s.leak())   // visible .leak() — flag at PR
Box::leak(format!(...).into_boxed_str()) // visible Box::leak — flag at PR
```

has the foothold to reject the PR.

The runtime checks in `from_static` (length cap of 64 chars, charset
`[a-zA-Z0-9_-]`) are belt-and-suspenders against the case where a
genuine static string drifts into transcript-shaped territory.

**`into_inner()` discipline.** `RedactedLabel::into_inner()` returns
the inner `String` because the `metrics::counter!` macro's label-value
APIs want `Into<Cow<'static, str>>`. The `String` is plain after
extraction, so a caller could `.push_str(...)` to it before emitting.
Mitigation: the call must be the **immediate** expression passed to
the metric macro:

```rust
metrics::counter!(
    NAME,
    "platform" => redacted!("zoom").into_inner(),  // OK — immediate
).increment(1);

// Reject in PR review:
let mut label = redacted!("zoom").into_inner();
label.push_str(&meeting.title);                    // bypasses validation
metrics::counter!(NAME, "platform" => label).increment(1);
```

The unit test
`heron_metrics::label::tests::redaction_unit_test_for_acceptance_criterion`
asserts that a transcript-shaped string is rejected by
`from_static` and that `hashed` produces a digest distinct from
the input.

### What to do when you need a high-cardinality dimension

You probably don't. Re-examine whether the metric is the right
shape:

- For per-meeting outcomes, prefer a counter labeled by an
  enum-shaped dimension (`platform`, `outcome`, `error_kind`)
  rather than `meeting_id`.
- For latency distributions, a histogram with a small label set
  is the right shape.
- If you genuinely need to correlate metrics with logs, emit the
  meeting id in the **log line** (`tracing::info!(meeting_id = %id, ...)`)
  not the metric label. The log-side surface is per-record and
  scoped to a session; the metric side is aggregate.

If after all that you still need an opaque correlation dimension,
use `RedactedLabel::hashed`. The output is a 64-bit FNV-1a digest;
this is **not cryptographic** — sufficient to bucket, insufficient
to defend against an attacker reconstructing the original.

**Cardinality warning.** Each distinct hashed input produces a
fresh time series in the Prometheus registry. Hashing every
`meeting_id` over a long-running daemon will exhaust memory.
`hashed()` is a last resort for cases where a small bounded set of
correlation keys is genuinely needed; if the input domain is
unbounded, the right answer is to drop the dimension.

**Threat-model fit.** `hashed()` is appropriate for opaque
unguessable IDs (UUIDv7 is 122 bits of randomness — a dictionary
attack is infeasible). It is NOT appropriate for hashing
dictionary-attackable values (participant names, emails, meeting
titles): an attacker with read access to the metrics endpoint plus
a wordlist can invert those. Don't put dictionary-attackable values
in metrics, period — `hashed()` is for opaque IDs, not as a fig
leaf for PII.

## Local exposure

The daemon mounts `GET /v1/__metrics` (Prometheus text exposition,
`Content-Type: text/plain; version=0.0.4`). Bearer-auth-gated like
every other non-`/health` route. The endpoint is intentionally not
listed in the public OpenAPI: the wire shape is Prometheus
exposition (not JSON), and clients should not treat it as a stable
contract.

The route is registered in
[`crates/herond/src/routes/metrics.rs`] and powered by
[`heron_metrics::MetricsHandle`] held in `AppState`.

### Inspecting locally

The bearer token is at `~/.heron/cli-token`:

```sh
TOKEN=$(cat ~/.heron/cli-token)
curl -s -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:7384/v1/__metrics
```

Expected output includes the smoke metric:

```text
# TYPE capture_started_total counter
capture_started_total{platform="zoom"} 1
```

To grep just the smoke metric:

```sh
curl -s -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:7384/v1/__metrics | grep capture_started_total
```

### Why not Tauri IPC / CLI subcommand?

Both alternatives were considered.

- **Tauri IPC** — would tie metric exposure to the desktop shell
  being open. The daemon is the long-running process; metrics
  should be reachable while the desktop is closed (e.g. for a
  diagnostics bundle or a CLI status check).
- **`heron-cli metrics` subcommand** — the CLI already speaks the
  daemon HTTP surface for every other operation. A subcommand
  could be added later as a thin curl wrapper, but it doesn't
  belong in the foundation issue.

The HTTP endpoint composes with both: a future Tauri command can
call into the daemon, and a CLI subcommand can render the same
endpoint.

## LLM call metrics (#225)

The LLM crate (`heron-llm`) instruments every `Summarizer::summarize`
call with the shared timing helper [`heron_metrics::timed_io_async`].
All four backends (`Anthropic`, `OpenAI`, `ClaudeCodeCli`, `CodexCli`)
emit the same metric shape, distinguished by the `op` / `backend`
labels:

| Metric | Type | Labels | Notes |
| --- | --- | --- | --- |
| `llm_call_duration_seconds` | histogram | `op` (= backend slug) | Wall-clock duration of the summarize call, including transcript read + render. |
| `llm_call_failures_total` | counter | `op`, `reason` | `reason` is enum-shaped — see [`LlmError::failure_reason`]. |
| `llm_tokens_input_total` | counter | `backend`, `model` | Folds prompt-cache fields per §11.4. |
| `llm_tokens_output_total` | counter | `backend`, `model` | Completion tokens. |
| `llm_cost_usd_micro_total` | counter | `backend`, `model` | Integer micro-USD (USD × 1 000 000); see "LLM cost counter shape" below. |

### LLM cost counter shape

Fractional dollars don't fit a Prometheus counter cleanly: counters
must be monotonic and integer-valued for `rate()` to make sense, but
the per-call cost (e.g. `$0.0123`) has 4 decimal places of precision.
We chose the **integer micro-USD counter** route over a histogram:

- The counter accumulates `compute_cost(...).summary_usd × 1_000_000`,
  rounded to the nearest integer. `compute_cost` already rounds to
  4 decimal places (see `heron_llm::cost::round_cents`), so the
  multiplication is exact-integer for every value the rate table
  produces. The 1_000_000 factor matches the `_micro_` unit prefix
  (1 USD = 10^6 micro-USD).
- Dashboards recover USD by dividing the counter rate by 1_000_000:
  `rate(llm_cost_usd_micro_total[5m]) / 1000000`.
- A histogram was rejected: `histogram_sum` is the only path to a
  per-bucket cost total, and Prometheus client libraries don't
  guarantee `_sum` is monotonic across cardinality changes (a label
  drop resets the bucket). A counter is the right shape for "total
  spend over time."

Critical privacy invariants for LLM metrics:

- **`backend` is a closed enum.** `anthropic` / `openai` /
  `claude_code_cli` / `codex_cli` — see
  [`heron_llm::metrics_labels::backend_label`].
- **`model` is a bounded bucket.** Every model the rate table
  recognizes maps to a `redacted!("…")` literal; an unknown model
  collapses to `redacted!("unknown_model")` so dashboards see a
  bounded label cardinality even if a future model ships before the
  table is updated.
- **No prompt text, persona text, response text, or transcript
  excerpts** appear in any label. The pinning is type-level via
  `RedactedLabel`; reviewers verifying this can grep the LLM crate
  for `redacted!` and confirm every call site uses a string literal.

## Vault I/O metrics (#225)

The vault writer (`heron-vault::writer`) and the orchestrator's
read-side projection (`heron-orchestrator::vault_read`) both
instrument their disk-touching code paths.

### Write side (`heron-vault`)

| Metric | Type | Labels |
| --- | --- | --- |
| `vault_write_duration_seconds` | histogram | `op` ∈ {`atomic_write`, `update_action_item`, `finalize`} |
| `vault_write_failures_total` | counter | `op`, `reason` (= [`VaultError::failure_reason`]) |

`finalize` and `update_action_item` emit a row at the high-level
operation boundary; the inner `atomic_write` calls also emit their
own rows under `op="atomic_write"` so a "what slowed down" panel can
drill from finalize → write.

### Read side (`heron-orchestrator::vault_read`)

| Metric | Type | Labels | Site |
| --- | --- | --- | --- |
| `vault_transcript_oversized_lines_skipped_total` | counter | none | Over-cap-line warn site in `read_transcript_segments`. |
| `vault_transcript_segments_count` | histogram | none | Per-call segment count on `read_transcript_segments` return. |
| `vault_transcript_bytes_read_bytes` | histogram | none | Per-call total bytes drawn off the file (including drained over-cap tails). |
| `vault_path_resolve_symlink_rejected_total` | counter | `field` ∈ {`transcript`, `recording`, …} | Symlink-reject site in `reject_symlinked_components`. |
| `bot_context_render_failed_total` | counter | `reason="too_large"` | `compose.rs` render-fail drop site. |

The read-side metrics share a privacy-sensitive design constraint
with the write side: the over-cap counter has **no per-meeting
label** because that would explode cardinality (one time series per
meeting). Operators correlate a counter spike with the matching
`tracing::warn!` log line.

The buffer-reuse caveat from PR #228 is preserved: the
`Vec::with_capacity(256)` allocation in `read_transcript_segments`
and its `buf.clear()` reset stay tied to the function scope; no
metrics-recording block wraps them.

### Shared timing helper

[`heron_metrics::timed_io_sync`] / [`heron_metrics::timed_io_async`]
ship the canonical "external/IO call wrapped with timing + outcome"
shape. Both LLM and vault write paths use it; the vault read paths
use raw `metrics::counter!` / `metrics::histogram!` calls because
their measurements (per-call segment count, oversize-line bumps) don't
fit the timing-wrapper shape.

The helper takes a `RedactedLabel` for the `op` dimension and
delegates to a `ClassifyFailure` impl on the error type for the
`reason` dimension on failures — both concrete types are pinned to
enum-like values, never user input.

## Smoke metric

`capture_started_total` is the canonical example sub-issues #224
/ #225 / #226 copy. It's instrumented in
[`heron_orchestrator::start_capture`](../crates/heron-orchestrator/src/lib.rs)
right after the FSM walks `armed → recording`:

```rust
let platform_label = match args.platform {
    Platform::Zoom => redacted!("zoom"),
    Platform::GoogleMeet => redacted!("google_meet"),
    // ...
};
metrics::counter!(
    SMOKE_CAPTURE_STARTED_TOTAL,
    "platform" => platform_label.into_inner(),
).increment(1);
```

The exported const `heron_metrics::SMOKE_CAPTURE_STARTED_TOTAL` is
what tests assert against, so a future rename of the smoke metric
flows through the test surface without manual sync.

## Adding a new metric (for #224 / #225 / #226)

1. **Pick a name** that obeys the convention. Declare it as a
   `const NAME: &str = metric_name!("...")` near the call site or in
   a module-level metrics block — the macro validates the convention
   at first call.
2. **Pick label dimensions.** Every label value MUST be a
   `RedactedLabel`. If a dimension you need can't be expressed with
   `redacted!("literal")` for an enum-like value, stop and ask
   whether you actually need that dimension.
3. **Wire the call site.** Use `metrics::counter!`, `histogram!`, or
   `gauge!` macros. Increment / observe / set at the boundary of
   the operation being measured (the `await` point for an async
   call, the spot in the FSM for a state transition).
4. **Test it.** A unit test that drives the call site once and
   asserts the metric appears in `MetricsHandle::render()` output is
   sufficient for the smoke-level assertion. The end-to-end
   `metrics_endpoint_returns_prometheus_exposition_with_bearer`
   test in `crates/herond/tests/api.rs` is the canonical shape.
5. **No new feature flags.** Metrics are foundational; adding a
   counter must not gate on a workspace feature.

## Deferred follow-ups

The following hardening items were called out by review and are
deferred to follow-up PRs rather than gating this foundation:

- **`.leak()` lint at CI level.** The known bypass on
  `RedactedLabel::from_static` is `Box::leak(format!(...))`. A
  `cargo deny` or grep-level workflow check that forbids `.leak()`
  outside an allowlisted set of crates would harden this further.
  Today it remains a manual review item.
- **Workspace metric-name registry.** As sub-issues #224 / #225 /
  #226 land, multiple crates will define metric names. A central
  registry (a single `static`-ed `phf::Map` or a unit test that
  walks the workspace's `metric_name!` invocations) would catch
  collisions before runtime. Today the convention is
  one-test-per-call-site.
- **Per-install secret for `hashed()`.** Using FNV-1a unkeyed is
  fine for the current threat model (opaque UUIDv7 inputs only).
  If a future use case ever genuinely needs to hash a
  dictionary-attackable input, switch `hashed()` to keyed BLAKE3
  with a per-install secret read from the same path as the bearer
  token. Today the docs forbid that use case at the source.

## Cross-references

- `crates/heron-metrics/src/lib.rs` — crate-level docs.
- `crates/heron-metrics/src/label.rs` — `RedactedLabel` invariants
  and tests.
- `crates/heron-metrics/src/naming.rs` — naming convention
  validator and tests.
- `crates/heron-metrics/src/recorder.rs` — Prometheus recorder
  install path.
- `crates/herond/src/routes/metrics.rs` — daemon `/v1/__metrics`
  route.
- `CLAUDE.md` §"Observability privacy" — the redaction rule that
  every reviewer must enforce on metric PRs.
