# Contributing

Conventions for changes inside this repo. v1 is a solo build; the
rules below exist so I (or a future-me, or an agent) can pick up
work mid-week and not break the contract every other crate is built
against.

## Where to start

Read `docs/architecture.md` for the current codebase shape. Historical
phase references live in `docs/archives/plan.md` and
`docs/archives/implementation.md`; older PRs reference those sections
(e.g. §6.2, §13.3), so preserve the mapping when touching code that
still cites them.

## Tooling

Run `./scripts/setup-dev.sh` to install the pinned toolchain and
system dependencies. The script is idempotent and prints what it
skips.

| Tool | Pin | Source |
|---|---|---|
| Rust toolchain (dev) | `1.90.0` | `rust-toolchain.toml` |
| Rust MSRV | `1.88` | `Cargo.toml` `rust-version` |
| Edition | `2024` | `Cargo.toml` |
| Bun | latest | per `apps/desktop/package.json` |
| Swift | system | `xcrun -f swiftc` |
| ffmpeg / ffprobe | brew | §0.1 prerequisite |
| cargo-deny | latest | `deny.toml` |

The toolchain pin is what we develop against; the MSRV is the
minimum the published crates compile under. `1.90` is needed for
let-chains in the matcher code; `1.88` is the floor below which
crates depending on us would break.

## Per-PR checklist

Every PR runs through the same pipeline:

1. **Implement** the change in a `worktree-<phase>-<slug>` branch.
2. **Polish** — three review agents, in parallel:
   - Code simplifier — looks for duplication / over-abstraction.
   - Code reviewer — looks for bugs, security, convention drift.
   - Ultrathink — extended-thinking pass for edge cases / migration risk.
3. **Apply** the substantive findings; defend the others in the PR
   reply thread. Skip the noise (style nits the formatter handles,
   over-engineering proposals).
4. **Local acceptance** — five gates, all must pass:
   ```sh
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all -- --check
   # desktop renderer (tsc IS the TS lint — no ESLint configured)
   (cd apps/desktop && bun run build)
   (cd apps/desktop && bun test)
   ```
5. **Open the PR** with the standard body (Summary / Polish findings
   applied / Acceptance / Test plan).
6. **CI** runs `test` (build + clippy + fmt + heron-doctor + heron-vault
   bridge), `markdown-lint`, `link-check`, `cargo-deny`, plus
   automated review by `gemini-code-assist` / CodeRabbit.
7. **Address review comments** in a follow-up commit, reply inline
   to each thread with the addressing-commit SHA.
8. **Squash-merge** once green and reviews are resolved.

This is automated; the polish + pr-workflow are wired as user-level
skills (`/polish`, `/pr-workflow`).

## Code style

- Rust 2024 edition; rustfmt with default config; clippy with
  `-D warnings` plus `-D clippy::expect_used` and `-D clippy::unwrap_used`
  enforced in non-test code.
- Default to no comments. Add one when the *why* is non-obvious — a
  hidden constraint, a workaround for a specific bug, behavior that
  would surprise a reader.
- Don't write comments that explain *what* well-named code already
  shows. Don't reference the current task / fix / caller — that
  belongs in the PR description and rots fast.
- Errors: `anyhow::Result` at binary boundaries, `thiserror::Error`
  on typed errors crossing crates. Prefer `String` over `&'static str`
  in error variants for consistency.
- File atomicity: every vault write goes through
  `heron_vault::atomic_write` (UUID temp + fsync + rename + 0600).
- File reads from untrusted sources: stream + cap line length.
  See `heron_doctor::log_reader::read_session_summaries` for the
  pattern.

## Swift bridges

Three live in `swift/`. New bridges mirror the canonical
`eventkit-helper` shape per `docs/archives/swift-bridge-pattern.md`:

- A `Package.swift` declaring a static library, no external Swift-
  package deps unless absolutely required (so `swift build` runs
  offline).
- One `Sources/<Helper>/<Helper>.swift` with three or four `@_cdecl`
  exports plus a paired `_free_string` for any malloc'd buffer.
- A Rust-side `<crate>/build.rs` cribbed from
  `crates/heron-vault/build.rs` (the canonical reference).
- A Rust-side `<crate>/src/<bridge>.rs` with pinned `_RAW`
  constants, an FFI-status enum carrying the raw `i32` in its
  `Internal` variant, and `#[cfg(not(target_vendor = "apple"))]`
  shims that return `NotYetImplemented`.
- Tests asserting each enum variant equals its pinned constant —
  drift fails CI rather than silently coercing to a stable variant.

## Tests

- Unit tests live next to the code they test (`#[cfg(test)] mod tests`).
- Integration tests in `crates/<crate>/tests/` are for cross-module
  contracts; gate anything heavy with `#[ignore]`.
- `// [needs-human]` tests record their artifact under
  `fixtures/manual-validation/<test-name>/<date>.{mov,wav,png}` and
  appear in `docs/archives/manual-test-matrix.md`.
- `cargo test` must pass with **zero** flaky tests. If timing is
  involved, prefer lower bounds (`>= X`) over upper bounds (`<= Y`)
  so CI's GitHub Actions runner doesn't false-fail on scheduler jitter.

## Commit messages

```
<type>(<scope>): <subject>

<body — what changed and why; reference §s in implementation.md>

<polish findings applied, if any>

<acceptance line>

Co-Authored-By: <agent identity>
```

Type prefixes: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
The agent identity for AI-assisted commits is documented in the
session memory (`memory/git_identity.md`).
