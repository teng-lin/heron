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
```

has the foothold to reject the PR.

The runtime checks in `from_static` (length cap of 64 chars, charset
`[a-zA-Z0-9_-]`) are belt-and-suspenders against the case where a
genuine static string drifts into transcript-shaped territory.

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
