# Heron testing roadmap

Status: 2026-04-30 — first slice of a multi-PR effort to close the
test-coverage gaps surfaced by the audit on the same day. Replaces an
earlier `docs/test-strategy.md` draft that two `/momus` review cycles
showed was accumulating technical bluffs faster than execution would
have caught them.

## Gaps the audit found

The codebase has solid Rust unit-test coverage and modest TypeScript
unit coverage via `bun test`. The seams are uncovered:

- **Tauri IPC** — Tauri commands cross TS ↔ Rust with no contract
  test. Live drift exists today: `heron_take_pending_shortcut_conflicts`
  is registered in `apps/desktop/src-tauri/src/lib.rs`'s
  `tauri::generate_handler!` invocation but has no `HeronCommands`
  binding in `apps/desktop/src/lib/invoke.ts`.
- **Settings schema migration** — Tier 1 added several
  `#[serde(default)]` fields with no fixture-driven round-trip test.
- **Vault writer** — RFC 7396 action-item patch semantics, iCloud
  lock retry, and hostile YAML inputs are untested through real disk
  I/O. Per-field unit tests exist; an integration test does not.
- **LLM backends** — OpenAI vs Anthropic dispatch (PR #178) is not
  exercised in CI; persona prompt-injection surface is untested.
- **Desktop UI** — no e2e coverage. Date-chip timezone math (PR #177),
  action-item rendering (PR #180), and the salvage flow are eyeballed
  only.
- **Real Clio pipeline** — `heron-audio/tests/end_to_end_real.rs` is
  `#[ignore]`d; `heron-speech/tests/whisperkit_real.rs` and
  `heron-llm/tests/live_api.rs` runtime-skip on missing env vars and
  pass as no-ops on CI.
- **Swift bridges** — `whisperkit-helper`, `eventkit-helper`, and
  `zoomax-helper` ship in production with zero XCTest coverage.
- **Tauri config** — `apps/desktop/src-tauri/tauri.conf.json` ships
  `assetProtocol.scope: ["**"]`, which lets the renderer resolve any
  path the app process can read.
- **CI gate alignment** — `CLAUDE.md` and `CONTRIBUTING.md` do not
  list `bun run build` + `bun test` as required gates, despite user
  policy requiring them.

## What ships first: desktop CI (this PR)

`.github/workflows/desktop.yml` runs `bun install --frozen-lockfile`,
`bun run build` (which is `tsc && vite build`), and `bun test` on
every PR. ubuntu-latest, no Rust touched.

Acceptance: a PR that breaks a `*.test.ts` file or fails `tsc` fails
CI.

## Deferred — questions, not answers

Each entry below is a question that needs prototype-first
investigation before any prescription is committed to text. Do not
specify a mechanism without running the relevant command first
(`npm view`, `cargo build --features ...`, `gh label create`, etc.).
Listed so they aren't lost; sequenced loosely by dependency.

- **IPC parity test.** Likely shape: `syn`-parse the
  `tauri::generate_handler!` macro args, emit a JSON manifest, assert
  `HeronCommands` matches. Cross-workflow file handoff is the open
  question — combining `rust.yml` and `desktop.yml` into one workflow
  may be simpler than `actions/upload-artifact` plumbing. Fix the
  existing `heron_take_pending_shortcut_conflicts` drift in the same
  PR.
- **IPC payload snapshots.** `insta` snapshot tests for the JSON
  shapes of `heron_summarize`, `heron_update_action_item`,
  `heron_get_meeting`, `heron_write_settings`, `heron_prepare_context`.
- **Settings migration fixtures.** Pre-Tier 1 + full-Tier 1 round-trip
  in `apps/desktop/src-tauri/tests/`.
- **Vault action-item integration.** Per-field RFC 7396 cases through
  real disk; iCloud lock retry; hostile-YAML round-trip. Item
  deletion needs an ADR before any test is written — the merge-patch
  spec doesn't define array-element removal.
- **LLM dispatch.** wiremock for both backends + body-shape snapshot
  (catches "OpenAI shape sent to Anthropic"). Persona-injection
  surface.
- **Desktop e2e.** Shipped (issue #191). Playwright + tauri-driver
  with two lanes: a renderer-only `smoke` project that mocks
  `window.__TAURI_INTERNALS__.invoke` via `addInitScript` (no Vite
  alias needed — the canonical Tauri pattern bypasses the import-graph
  surgery `mock.module` does in Bun unit tests), and a `tauri`
  project run nightly against the packaged `@crabnebula/tauri-driver`
  binary (NOT `@tauri-apps/tauri-driver`, which 404s on npm). The
  smoke lane covers `app-launch` + `settings-roundtrip`; the
  remaining flows from the issue (`onboarding`, `recording`,
  `review-rail`, `timezone`, `action-item-edit`, `salvage`) are
  scheduled for follow-up issues — adding them all at once would
  blow the ≤2 minute wall-time budget the issue spec sets.
- **Real-pipeline nightly.** Synthetic ~3 MB audio fixture (no LFS
  needed at that size), a `real-pipeline` Cargo feature whose
  workspace propagation behavior needs experimental verification, and
  a `gh run list`-driven consecutive-failure detector. The
  `nightly-failure` issue label must be created first; nightly
  workflow needs `issues: write` permission.
- **Swift XCTest.** A `Tests/` target per Swift package. WhisperKit
  has a network dependency to plan around in CI.
- **Tauri scope hardening.** `["**"]` is too wide. Static config
  cannot enumerate user-configurable vault paths — `Settings::default`
  sets `vault_root: String::new()` and the runtime
  `resolve_vault_root` in `lib.rs` falls back to `~/heron-vault` only
  when the stored value is empty. Narrowing likely needs the runtime
  `Scope::allow_directory` API rather than a `tauri.conf.json` edit.
- **CLAUDE.md / CONTRIBUTING.md alignment.** List the Bun gates
  explicitly, and add a CI step that diffs the gate sets in the two
  docs to prevent future drift.

## Why this shape

Two prior plan rewrites accumulated technical bluffs that survived
three rounds of multi-agent review. Each cycle caught ~12 fresh
issues — broken regexes, nonexistent npm packages, inert TS type
assertions, missing cross-workflow plumbing — that 5 minutes of
shell commands resolved on inspection. The lesson, captured: for
infrastructure work in this repo, plan the smallest verifiable PR
and let what works rewrite the next plan. The deferred list above is
honest about being unproven; entries graduate to PRs as their
prerequisites get prototyped.
