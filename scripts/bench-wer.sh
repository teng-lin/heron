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
declare -A WHISPERKIT_THRESHOLD=(
    ["client-3person-gallery"]="15.0"
    ["team-5person-with-dialin"]="22.0"
    ["1on1-internal"]="12.0"
)
declare -A SHERPA_THRESHOLD=(
    ["client-3person-gallery"]="22.0"
    ["team-5person-with-dialin"]="30.0"
    ["1on1-internal"]="18.0"
)

wk_threshold=${WHISPERKIT_THRESHOLD[$fixture]:-}
sh_threshold=${SHERPA_THRESHOLD[$fixture]:-}
if [[ -z "$wk_threshold" || -z "$sh_threshold" ]]; then
    echo "unknown fixture: $fixture (no §8.5 threshold)" >&2
    exit 1
fi

# Sanity-check fixture layout per fixtures/speech/README.md.
for f in mic.wav tap.wav ground-truth.jsonl README.md; do
    if [[ ! -e "$fixture_dir/$f" ]]; then
        echo "fixture incomplete: missing $fixture_dir/$f" >&2
        # placeholder: don't fail on layout in v0; warn only.
        echo "  (warning: bench-wer placeholder skips this case)" >&2
        exit 0
    fi
done

echo "fixture: $fixture"
echo "thresholds: whisperkit ≤ ${wk_threshold}%   sherpa ≤ ${sh_threshold}%"

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
