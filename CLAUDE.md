# heron — Claude workflow

## Per-PR workflow (mandatory)

For every PR you ship in this repo, run this sequence end-to-end:

1. **`/polish`** — three sequential stages, the same ones `CONTRIBUTING.md` §"Per-PR checklist" calls out:
   1. Code simplifier — duplication / over-abstraction.
   2. Multi-model code review — Claude + Codex + Gemini in parallel via the `pr-review` skill / their CLIs.
   3. Ultrathink — extended-thinking pass for edge cases / migration risk.
   Apply consensus fixes; defer architectural items with explicit reasoning.
2. **`/pr-workflow`.** Commit on a feature branch, push, create the PR with the standard summary + test-plan template.
3. **Address every reviewer comment.** Fix in code, push, then **reply to each review thread** with the fix SHA — even nitpicks. Do not leave open threads.
4. **Resolve merge conflicts before merging.** If `mergeStateStatus` is anything other than `CLEAN`, rebase or merge `main` in, re-run tests, and re-push.
5. **Merge the PR.** Only after CI is fully green AND all review threads are resolved AND `mergeStateStatus: CLEAN`. Use `gh pr merge --squash --delete-branch` unless the user has set a different default.

This applies to every PR the assistant opens — feature work, fixes, docs, refactors. Skip nothing.

## Verification before commit (mandatory)

The same gates `CONTRIBUTING.md` §"Per-PR checklist" item 4 enforces — running them workspace-wide catches regressions in crates that depend on what you touched:

- `cargo test --workspace` passes
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- `bun run build` in `apps/desktop/` passes (tsc + vite build; tsc IS the TS lint — no ESLint configured)
- `bun test` in `apps/desktop/` passes

**Known local exception:** `heron-cli`'s test binary fails to load `libonnxruntime.1.17.1.dylib` on this machine, surfacing as a `dyld` SIGABRT in `cargo test --workspace` runs. This is environmental, not a code bug. If your change touches `heron-cli` directly, fix the dylib path (or test in CI). Otherwise, treat that single failure as preexisting and confirm the rest of the workspace is green.
