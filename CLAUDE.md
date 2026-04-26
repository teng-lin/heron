# heron — Claude workflow

## Per-PR workflow (mandatory)

For every PR you ship in this repo, run this sequence end-to-end:

1. **`/polish` with three review agents.** Stage 1 simplifier, then stage 2 multi-model review (Claude + Codex + Gemini in parallel via the `pr-review` skill / their CLIs), then stage 3 ultrathink. Apply consensus fixes; defer architectural items with explicit reasoning.
2. **`/pr-workflow`.** Commit on a feature branch, push, create the PR with the standard summary + test-plan template.
3. **Address every reviewer comment.** Fix in code, push, then **reply to each review thread** with the fix SHA — even nitpicks. Do not leave open threads.
4. **Resolve merge conflicts before merging.** If `mergeStateStatus` is anything other than `CLEAN`, rebase or merge `main` in, re-run tests, and re-push.
5. **Merge the PR.** Only after CI is fully green AND all review threads are resolved AND `mergeStateStatus: CLEAN`. Use `gh pr merge --squash --delete-branch` unless the user has set a different default.

This applies to every PR the assistant opens — feature work, fixes, docs, refactors. Skip nothing.

## Verification before commit (mandatory)

- `cargo test -p <touched-crates>` passes
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean

The `heron-cli` test suite has a known dylib loading issue (`libonnxruntime.1.17.1.dylib`) on this machine; if your change doesn't touch `heron-cli`, that failure is preexisting and not blocking.
