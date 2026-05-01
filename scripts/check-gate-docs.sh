#!/usr/bin/env bash
# check-gate-docs.sh — verify the pre-PR gate lists in CLAUDE.md and
# CONTRIBUTING.md stay in sync. CI fails on drift so the two docs
# never give contributors conflicting guidance.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# CLAUDE.md "Verification before commit" lists each gate as
#   - `<cmd>` <prose>
# Capture the text between the first pair of backticks on each bullet.
extract_claude_gates() {
    awk '
        /^## Verification before commit/ { in_section = 1; next }
        in_section && /^## / { in_section = 0 }
        in_section && /^[[:space:]]*-[[:space:]]+`/ {
            line = $0
            sub(/^[[:space:]]*-[[:space:]]+`/, "", line)
            sub(/`.*$/, "", line)
            print line
        }
    ' CLAUDE.md
}

# CONTRIBUTING.md item 4 "Local acceptance" puts each gate on its own
# line inside a fenced code block. Strip leading indent, the
# `(cd apps/desktop && ` wrapper, and the trailing `)`.
extract_contributing_gates() {
    awk '
        /^4\. \*\*Local acceptance/ { in_section = 1; next }
        in_section && /^[0-9]+\. / { in_section = 0 }
        in_section && /^[[:space:]]*```/ { in_block = !in_block; next }
        in_section && in_block && !/^[[:space:]]*#/ && !/^[[:space:]]*$/ {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            sub(/[[:space:]]+$/, "", line)
            if (sub(/^\(cd apps\/desktop && /, "", line)) {
                sub(/\)[[:space:]]*$/, "", line)
            }
            print line
        }
    ' CONTRIBUTING.md
}

claude=$(extract_claude_gates | sort)
contributing=$(extract_contributing_gates | sort)

# Guard against silent pass when section headers/formatting drift —
# two empty extractions would otherwise compare equal and CI would
# go green with nothing actually verified.
if [ -z "$claude" ]; then
    echo "✘ no gates extracted from CLAUDE.md — section header or bullet format may have drifted" >&2
    exit 1
fi
if [ -z "$contributing" ]; then
    echo "✘ no gates extracted from CONTRIBUTING.md — section header or code-block format may have drifted" >&2
    exit 1
fi

if [ "$claude" = "$contributing" ]; then
    echo "✓ pre-PR gate lists in CLAUDE.md and CONTRIBUTING.md match:"
    echo "$claude" | sed 's/^/  /'
    exit 0
fi

echo "✘ pre-PR gate lists diverged between CLAUDE.md and CONTRIBUTING.md" >&2
echo "" >&2
echo "CLAUDE.md gates:" >&2
echo "$claude" | sed 's/^/  /' >&2
echo "" >&2
echo "CONTRIBUTING.md gates:" >&2
echo "$contributing" | sed 's/^/  /' >&2
echo "" >&2
echo "diff (< CLAUDE.md, > CONTRIBUTING.md):" >&2
diff <(printf '%s\n' "$claude") <(printf '%s\n' "$contributing") >&2 || true
exit 1
