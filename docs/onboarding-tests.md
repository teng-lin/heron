# Onboarding tests

heron does **not** use a VM for onboarding validation. Tests run on
the development machine; `scripts/reset-onboarding.sh` (per §0.4)
drops TCC grants and local state to simulate a first-run experience
between walkthroughs.

Source-of-truth is [`docs/implementation.md`](implementation.md) §5.5
(week 1 smoke test) and §13.5 (full week-11 validation).

## What "onboarding" covers

heron asks the user for four macOS permissions:

| Permission | Reason | Bucket name (`tccutil`) |
|---|---|---|
| Microphone | record the user's voice | `Microphone` |
| System Audio Recording | tap the meeting app's output | `AudioCapture` |
| Accessibility | read Zoom's AX tree for speaker names | `Accessibility` |
| Calendar | auto-fill attendees from the user's calendar (optional) | `Calendar` |

Each is requested at the relevant **step** of the 5-step onboarding
flow that ships in week 11 (§13.2). Onboarding tests verify that:

1. Each prompt actually fires (i.e. heron's Info.plist usage strings
   are wired correctly).
2. Each step can be **failed** in a recoverable way — the user can
   open System Settings, deny the prompt, return to heron, and the UI
   surfaces a clear "this is required" path.

## Smoke test (end of week 1)

Goal: confirm `tccutil reset` actually clears the bucket and the
prompt re-appears on relaunch. If it doesn't, week 11's full test
matrix can't validate anything, so we catch it now.

```sh
# 1. Reset all heron-relevant TCC + cache state.
scripts/reset-onboarding.sh

# 2. Open the empty Tauri shell built in week 2 (§6.4 placeholder
#    until the real shell ships in §13). Click "Test microphone."
#    Expected: macOS displays the Microphone TCC prompt.

# 3. Click "Allow." Run the reset script again.
scripts/reset-onboarding.sh

# 4. Re-launch the app, click the same button. Prompt should fire
#    *again* — proving the reset actually invalidated the prior grant.
```

Failure modes and what they mean:

| Symptom | Likely cause |
|---|---|
| `tccutil` errors with `unrecognized service` | macOS version too old; heron requires 14+ |
| Prompt appears the first time but not after reset | Bundle ID mismatch — the running binary's bundle ID doesn't match `com.heronnote.heron` |
| No prompt at all | Missing `NSMicrophoneUsageDescription` (or equivalent) in `Info.plist` — see §0.6 |

## Full validation (week 11)

12 walkthroughs total per §13.5: 5 positive paths + 5 counter-tests
(deny, partial-deny, etc.) + 2 edge cases (paginated gallery, slow-
network model download). Each ~3 min + ~1 min reset overhead.

Each walkthrough produces a screencast committed to
`fixtures/manual-validation/onboarding/<date>-<test>.mov`. The
matrix lives in [`docs/manual-test-matrix.md`](manual-test-matrix.md)
row 7.

## Coverage limits — explicitly accepted

- `reset-onboarding.sh` does NOT simulate a fresh user account or a
  fresh Mac. The author's machine has dev tools, network access,
  hardware peripherals, and an existing Apple ID session.
- Real naive-user coverage (UX confusion, copy clarity, default
  behavior on a stock Mac) moves to **week 16 exec dogfood**. Bugs
  surfacing there are accepted v1.1 candidates if they don't block
  the §18.2 ship criteria.

## Bundle ID override

The script targets `com.heronnote.heron` by default. Override for
local testing of a non-default build:

```sh
HERON_BUNDLE_ID=com.heronnote.test-foreign scripts/reset-onboarding.sh
```

This is also how the §6.5 keychain ACL test (week 2) resets the two
bundle IDs separately.
