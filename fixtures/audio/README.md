# `fixtures/audio/`

Long-form (≥ minute scale) audio fixtures used by the `real-pipeline`
nightly tests in `crates/herond/tests/clio_full_pipeline.rs`. These
fixtures are committed (small enough — < 5 MB — that git-LFS isn't
needed) so the nightly CI run reaches for the same bytes a contributor
running the test locally does.

## `clio-smoke-90s.wav`

**Status:** **Deferred — not yet committed.** Per issue #194 this
fixture is a 90-second mono 16 kHz 16-bit PCM WAV (~ 2.9 MB) generated
from a permissively licensed TTS source. The `clio_full_pipeline` test
gates on `feature = "real-pipeline"` AND env-var presence, so the
nightly workflow will skip-with-message until this is committed.

### Generating from Piper TTS (preferred — MIT-licensed)

[Piper](https://github.com/rhasspy/piper) ships under MIT and produces
clean 16 kHz speech that exercises both STT and the LLM
summarization path on a deterministic transcript.

```sh
# 1. Install Piper. macos-14 runners need to fetch the prebuilt:
curl -L -o piper.tar.gz \
  https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_macos_aarch64.tar.gz
tar -xzf piper.tar.gz
# 2. Pick a permissively-licensed voice (en_US-lessac-medium is CC0):
curl -L -O https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx
curl -L -O https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json
# 3. Synthesize the fixture transcript:
cat fixtures/audio/clio-smoke-90s.txt | \
  ./piper/piper --model en_US-lessac-medium.onnx --output_file fixtures/audio/clio-smoke-90s.wav
# 4. The output is 22.05 kHz; downsample to 16 kHz mono 16-bit:
ffmpeg -y -i fixtures/audio/clio-smoke-90s.wav \
  -ar 16000 -ac 1 -sample_fmt s16 \
  fixtures/audio/clio-smoke-90s.wav.tmp \
  && mv fixtures/audio/clio-smoke-90s.wav.tmp fixtures/audio/clio-smoke-90s.wav
```

### Source transcript (`clio-smoke-90s.txt`)

The fixture transcript is the deterministic input the nightly test
asserts the LLM summarizer can convert into a non-empty summary +
parsed action items. Held in a sibling `.txt` so a Piper / Coqui /
eSpeak NG run is reproducible. **Also deferred** until the WAV is
committed.

### License

`en_US-lessac-medium` (Lessac dataset) is **public domain (CC0)**;
Piper itself is **MIT-licensed**. The synthesized audio is therefore
free of any restriction beyond the credit Piper's
[`MODEL_CARD`](https://huggingface.co/rhasspy/piper-voices/blob/main/en/en_US/lessac/medium/MODEL_CARD)
asks downstream users to give. Record any attribution required by the
voice card here when committing the fixture.

## Why deferred

Generating a high-quality, license-clean TTS fixture requires either
the Piper toolchain installed locally or a manual one-time download.
Per the per-PR rule "do not commit a placeholder — partial fixtures
break tests", the fixture is left out until a committer with Piper
generates it from the published transcript. The nightly workflow's
real-pipeline job carries an env-var skip so a missing fixture does
not break the run.
