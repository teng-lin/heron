#!/usr/bin/env bash
# Reset all heron-relevant TCC grants and local state, simulating a
# first-run install for onboarding validation.
#
# Per docs/implementation.md §0.4 + §5.5. Run between onboarding
# walkthroughs in week 1 (smoke test) and week 11 (full validation).
#
# Coverage: this is NOT a fresh user account or fresh Mac. The author's
# laptop has dev tools, network access, hardware peripherals, and an
# existing Apple ID session. Real naive-user coverage moves to the
# week-16 exec dogfood per docs/manual-test-matrix.md.

set -euo pipefail

BUNDLE_ID="${HERON_BUNDLE_ID:-com.heronnote.heron}"

echo "Resetting TCC grants for ${BUNDLE_ID}..."
# tccutil exits non-zero if the bundle has never had the bucket
# granted. Treat that as success — it's the first-install state we
# wanted to simulate anyway.
tccutil reset Microphone     "${BUNDLE_ID}" || true
tccutil reset AudioCapture   "${BUNDLE_ID}" || true
tccutil reset Accessibility  "${BUNDLE_ID}" || true
tccutil reset Calendar       "${BUNDLE_ID}" || true

echo "Clearing app preferences and caches..."
rm -f  "${HOME}/Library/Preferences/${BUNDLE_ID}.plist"
rm -rf "${HOME}/Library/Application Support/${BUNDLE_ID}"
rm -rf "${HOME}/Library/Caches/heron"

echo "Reset complete. Re-launch heron — TCC prompts should re-appear."
