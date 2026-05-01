# `fixtures/audio/`

Long-form (≥ minute scale) audio fixtures used by the `real-pipeline`
nightly tests in `crates/herond/tests/clio_full_pipeline.rs`. These
fixtures are committed (small enough — < 5 MB — that git-LFS isn't
needed) so the nightly CI run reaches for the same bytes a contributor
running the test locally does.

## `clio-smoke-90s.wav`

A 90-second mono 16 kHz 16-bit PCM WAV (~2.8 MB) generated from the
sibling `clio-smoke-90s.txt` transcript via macOS's built-in `say` TTS,
then downsampled with ffmpeg. The transcript is hand-written to
exercise the action-item extraction pathway: it contains four explicit
"Action item: ..." sentences over a synthetic standup-style sync.

**Audio:** mono / 16 kHz / 16-bit PCM / 90.18 s / 2 818 KiB.
**SHA256 (`clio-smoke-90s.wav`):**
`95fd82246e1e673c96c4b7f6ef4f427793818ef14dd6c481dc990ae3a2f0918f`
**SHA256 (`clio-smoke-90s.txt`):**
`9696e32a54c98443f288b3bc895250af73dd66b947f9fc3926f99477ae29f939`

If you regenerate either file, update both hashes here so a stale
fixture surfaces in code review.

### Regenerating from macOS `say` (current canonical path)

This is the only path verified to work on a clean macOS Apple Silicon
dev machine in 2026. Requires `say` (Apple stock) and `ffmpeg`
(`brew install ffmpeg`).

```sh
# 1. Synthesize 22.05 kHz LEI16 raw WAV from the transcript.
say -v Samantha \
    -o /tmp/clio-raw.wav \
    --data-format=LEI16@22050 \
    -f fixtures/audio/clio-smoke-90s.txt

# 2. Downsample to the test's required mono / 16 kHz / 16-bit PCM shape.
ffmpeg -y -i /tmp/clio-raw.wav \
    -ar 16000 -ac 1 -sample_fmt s16 \
    fixtures/audio/clio-smoke-90s.wav

# 3. Refresh the SHA256 line in this README.
shasum -a 256 fixtures/audio/clio-smoke-90s.wav
```

`Samantha` is shipped on every modern macOS install; no separate
download is required. If you want to swap voices, pick another stock
US-English voice (`say -v '?' | grep en_US`) and document the change in
the regeneration commit.

#### License (path 1)

macOS users are licensed to use Apple's bundled voices, and the
synthesized output here is the user's. The voice **model itself** is
not redistributed — only the synthesized 90-second WAV is committed,
which is permissible under the macOS license. This fixture is intended
for functional testing only.

### Why this isn't Piper anymore

The README's previous instructions pointed at the Piper TTS release
`piper_macos_aarch64.tar.gz` from rhasspy/piper's `2023.11.14-2`
release. As discovered while resolving issue #216, that tarball
**ships an x86_64 binary mislabeled as aarch64** (Rosetta runs it) and
**is missing `libespeak-ng.1.dylib`** — `piper --help` exits with
`dyld[…]: Library not loaded: @rpath/libespeak-ng.1.dylib`. The Piper
macOS arm64 distribution is broken on a clean 2026 dev machine, so the
documented path was unreproducible.

Two alternatives remain on the table if `say` ever regresses:

1. **Piper from a working build.** Either build from source or wait
   for a maintained release that bundles `libespeak-ng.1.dylib`. If you
   verify a working release, swap path 1 above out for the Piper
   recipe and link the release tag here.
2. **Public-domain audio (Internet Archive / LibriVox).** Download a
   90-second span of a CC0 audiobook with `curl`, downsample with
   `ffmpeg`. Real human speech, harder to control what the LLM
   summarizer sees, but trivially license-clean. Useful for CI lanes
   that don't have macOS available.

## `clio-smoke-90s.txt`

The deterministic source transcript for the WAV above. Hand-written
synthetic standup content: four explicit "Action item: …" sentences
covering an onboarding redesign, a billing migration, an all-hands
prep, and a security-audit triage. The LLM summarizer in the
`clio_full_pipeline` test should extract these into the structured
`frontmatter.action_items` rows the desktop renderer's Actions tab
reads.

The transcript is committed (not regenerated) so a Piper / `say` /
Coqui run is reproducible from the same bytes.
