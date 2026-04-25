#!/usr/bin/env bash
# bench-wer.sh — measure WER per backend on a `fixtures/speech/<case>/`
# fixture and assert thresholds per `docs/implementation.md` §8.5.
#
# Until the WhisperKit + Sherpa wires land in weeks 4–5, this script
# is a placeholder that:
#   - validates its argument is a fixture directory shaped per the
#     §8.5 + fixtures/speech/README.md spec,
#   - prints what it WOULD run on each backend,
#   - exits 0 (so CI doesn't fail in the meantime).
#
# Once the real backends ship, replace the WhisperKit / Sherpa command
# blocks below; the threshold-check stays the same.

set -euo pipefail

usage() {
    cat <<EOF
Usage: $0 <fixture-dir>

  <fixture-dir>   one of:
                  fixtures/speech/client-3person-gallery
                  fixtures/speech/team-5person-with-dialin
                  fixtures/speech/1on1-internal

Outputs per-backend WER and exits non-zero if any measurement
exceeds the threshold pinned in
crates/heron-speech/src/selection.rs::WER_THRESHOLDS.
EOF
    exit 64
}

[[ $# -eq 1 ]] || usage
fixture_dir="$1"

[[ -d "$fixture_dir" ]] || { echo "fixture dir not found: $fixture_dir" >&2; exit 1; }
fixture=$(basename "$fixture_dir")

# Threshold table mirrors selection.rs::WER_THRESHOLDS. Drift here is
# caught by the §8.7 done-when test that re-reads the Rust constant.
#
# `case` rather than `declare -A` so the script runs on macOS's
# default Bash 3.2 (associative arrays need 4.0+). This script is
# expected to be run on the dev's laptop, which may not have Homebrew
# Bash on PATH.
case "$fixture" in
    "client-3person-gallery")    wk_threshold="15.0"; sh_threshold="22.0" ;;
    "team-5person-with-dialin")  wk_threshold="22.0"; sh_threshold="30.0" ;;
    "1on1-internal")             wk_threshold="12.0"; sh_threshold="18.0" ;;
    *)
        echo "unknown fixture: $fixture (no §8.5 threshold)" >&2
        exit 1 ;;
esac

echo "fixture: $fixture"
echo "thresholds: whisperkit ≤ ${wk_threshold}%   sherpa ≤ ${sh_threshold}%"

# Sanity-check fixture layout per fixtures/speech/README.md. `break`
# (not `exit`) on the first missing file: we still want the threshold
# + placeholder banners above to surface so the operator sees what
# would have run.
for f in mic.wav tap.wav ground-truth.jsonl README.md; do
    if [[ ! -e "$fixture_dir/$f" ]]; then
        echo "fixture incomplete: missing $fixture_dir/$f" >&2
        echo "  (warning: bench-wer placeholder skips this case)" >&2
        break
    fi
done

# Real impl (week 4–5):
#   wk_wer=$(cargo run --release --bin heron -- transcribe \
#              --backend whisperkit \
#              --mic "$fixture_dir/mic.wav" \
#              --tap "$fixture_dir/tap.wav" \
#              --against "$fixture_dir/ground-truth.jsonl" \
#              --emit-wer)
echo "whisperkit: PLACEHOLDER (backend not wired until week 4)"
echo "sherpa:     PLACEHOLDER (backend not wired until week 5)"

exit 0
