//! Streaming transcript reader shared by every summarizer backend.
//!
//! Owns the size + per-line caps and the over-cap-line drain logic.
//! Pulled out of `anthropic.rs` in phase 38 so the subprocess CLI
//! backends (`claude_code.rs`, `codex.rs`) reuse the exact same
//! reader without duplicating the byte-counting fix from PR #38.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::LlmError;

/// Hard cap on transcript file size when assembling the prompt. ~4
/// MiB easily covers a 4-hour meeting at 200 wpm and protects against
/// a runaway writer dumping the entire WAV stream into the JSONL.
pub const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;

/// Hard cap on a single transcript JSONL line. CONTRIBUTING.md
/// "stream + cap line length" — anything longer is treated as a
/// malformed entry and dropped from the prompt.
pub const MAX_TRANSCRIPT_LINE_BYTES: u64 = 64 * 1024;

/// Soft pre-flight warning threshold. Transcripts larger than this
/// approach the model's input-token limit (Sonnet 4.6 caps at 200K
/// tokens ≈ ~800 KiB of dense JSONL).
pub const TRANSCRIPT_WARN_BYTES: u64 = 600 * 1024;

/// Read a transcript JSONL file with size + per-line caps.
///
/// **Trust model.** The transcript path is supplied by the
/// orchestrator from heron's own writer, so the file is treated as
/// trusted: `File::open` follows symlinks and `metadata().len()` is
/// inspected before the open completes the stream. Callers that
/// might receive untrusted paths (third-party CLI consumers, future
/// MCP server inputs) should validate the path against the vault
/// root + cache root before invoking this.
///
/// `total` byte counter includes both successfully-read lines AND
/// drained tails of over-cap lines — fixes the PR-#36 review finding
/// where a file made of repeated 64-KiB+ lines could read past the
/// `MAX_TRANSCRIPT_BYTES` cap.
pub fn read_transcript_capped(path: &Path) -> Result<String, LlmError> {
    let file = File::open(path)
        .map_err(|e| LlmError::Backend(format!("open transcript {p:?}: {e}", p = path)))?;
    let len = file
        .metadata()
        .map(|m| m.len())
        .unwrap_or(MAX_TRANSCRIPT_BYTES + 1);
    if len > MAX_TRANSCRIPT_BYTES {
        return Err(LlmError::Backend(format!(
            "transcript {path:?} is {len} bytes, exceeds {cap}-byte cap",
            cap = MAX_TRANSCRIPT_BYTES
        )));
    }

    let mut reader = BufReader::new(file);
    let mut out = String::with_capacity(len as usize);
    // Reuse one buffer across iterations: `read_until` appends, so a
    // `clear()` at the top of each loop reuses the existing
    // allocation rather than dropping/re-allocating per line.
    let mut line: Vec<u8> = Vec::with_capacity(MAX_TRANSCRIPT_LINE_BYTES as usize + 1);
    let mut total = 0u64;
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take(MAX_TRANSCRIPT_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(LlmError::Io)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if line.len() as u64 > MAX_TRANSCRIPT_LINE_BYTES {
            let drained = consume_until_newline(&mut reader)?;
            total = total.saturating_add(drained);
            if total >= MAX_TRANSCRIPT_BYTES {
                break;
            }
            continue;
        }
        let s = std::str::from_utf8(&line)
            .map_err(|e| LlmError::Backend(format!("transcript {path:?} non-UTF-8: {e}")))?;
        out.push_str(s);
        if total >= MAX_TRANSCRIPT_BYTES {
            break;
        }
    }
    Ok(out)
}

/// Drain the rest of the current line without allocating, returning
/// the byte count consumed so the caller can keep its
/// [`MAX_TRANSCRIPT_BYTES`] tally accurate. Mirrors the canonical
/// pattern in `heron_doctor::log_reader`: a `fill_buf`/`consume`
/// loop walks the BufReader's internal buffer one chunk at a time,
/// so memory stays bounded at the buffer size regardless of how
/// long the over-cap line is.
fn consume_until_newline<R: BufRead>(reader: &mut R) -> Result<u64, LlmError> {
    let mut total_consumed: u64 = 0;
    loop {
        let (consumed, found_newline) = {
            let buf = reader.fill_buf().map_err(LlmError::Io)?;
            if buf.is_empty() {
                return Ok(total_consumed);
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(idx) => (idx + 1, true),
                None => (buf.len(), false),
            }
        };
        reader.consume(consumed);
        total_consumed = total_consumed.saturating_add(consumed as u64);
        if found_newline {
            return Ok(total_consumed);
        }
    }
}

/// Wrap `transcript_text` in the prompt + transcript layout every
/// summarizer backend uses. Pulling this out keeps the wire shape
/// identical across Anthropic API and CLI backends so a backend
/// change doesn't accidentally retrain users on a different format.
pub fn build_user_content(prompt: &str, transcript_text: &str) -> String {
    format!("{prompt}\n\nTranscript JSONL (one turn per line):\n```\n{transcript_text}\n```")
}

/// Tier 4 #21: privacy transform that walks the transcript JSONL line
/// by line, replacing each unique `speaker` value with a stable
/// `Speaker A` / `Speaker B` / … pseudonym. Letters are assigned in
/// first-appearance order so the same speaker maps to the same letter
/// across every call, which keeps re-summarize stable when the
/// underlying transcript hasn't changed.
///
/// ## Non-goals
///
/// - The strip applies *only* to the LLM input. The orchestrator's
///   round-trip `attendees` list still uses the real names — re-read
///   from the prior summary's frontmatter via `read_prior_items`.
/// - The "me" / "them" sentinels heron writes for self / unattributed
///   channel turns are preserved unchanged: stripping them would
///   collapse the user's own voice into the same pseudonym as a
///   remote speaker, which the §11.2 prompt template counts on for
///   speaker attribution.
/// - Lines that fail to parse as JSON or that don't carry a
///   `"speaker"` string field pass through verbatim. The aligner
///   never emits malformed lines, but a corrupted file shouldn't
///   poison the whole prompt — better to leak the raw speaker name
///   on a single bad line than to drop the entire transcript.
///
/// ## Letter overflow (>26 unique speakers)
///
/// After "Speaker Z", the next pseudonym is "Speaker AA", "Speaker AB",
/// …, "Speaker AZ", "Speaker BA", … (Excel-style base-26-without-zero).
/// Real meetings rarely have more than 5–10 distinct speakers, but the
/// fallback exists so a misbehaving aligner that emits a fresh label
/// per line never panics or wraps.
pub fn strip_speaker_names(transcript_text: &str) -> String {
    use std::collections::HashMap;

    // Sentinel labels heron's writer uses for its own bookkeeping
    // (mic = self, channel-only = unknown remote). These must not
    // collapse together — preserving them upstream preserves the
    // §11.2 attribution contract the prompt template depends on.
    fn is_sentinel(label: &str) -> bool {
        matches!(label, "me" | "them" | "")
    }

    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut next_index: usize = 0;
    let mut out = String::with_capacity(transcript_text.len());

    for line in transcript_text.split_inclusive('\n') {
        // `split_inclusive('\n')` keeps the trailing `\n` on every
        // intermediate line and yields the final partial line (no
        // `\n`) as a separate item, so we don't have to special-case
        // the file's last line.
        let trimmed_for_parse = line.trim_end_matches(['\n', '\r']);
        if trimmed_for_parse.is_empty() {
            out.push_str(line);
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed_for_parse) {
            Ok(serde_json::Value::Object(mut obj)) => {
                let needs_rewrite = matches!(
                    obj.get("speaker"),
                    Some(serde_json::Value::String(s)) if !is_sentinel(s)
                );
                if !needs_rewrite {
                    // Sentinel speaker / missing speaker / non-string
                    // speaker — pass the line through verbatim. Re-
                    // serializing here would reorder the JSON object's
                    // keys (default serde_json::Map is `BTreeMap`-
                    // backed without the `preserve_order` feature),
                    // which would silently change the byte shape of the
                    // transcript fed to the LLM even when no real-name
                    // replacement was needed.
                    out.push_str(line);
                    continue;
                }
                if let Some(serde_json::Value::String(name)) = obj.get("speaker").cloned() {
                    let pseudo = mapping
                        .entry(name)
                        .or_insert_with(|| {
                            let label = pseudonym_for_index(next_index);
                            next_index += 1;
                            label
                        })
                        .clone();
                    obj.insert("speaker".into(), serde_json::Value::String(pseudo));
                }
                // `to_string` doesn't pretty-print and preserves the
                // JSONL one-line-per-turn contract.
                out.push_str(
                    &serde_json::to_string(&serde_json::Value::Object(obj))
                        .unwrap_or_else(|_| trimmed_for_parse.to_owned()),
                );
                // Preserve the original line ending (`\n` or `\r\n`)
                // so downstream byte-counting stays accurate.
                if let Some(stripped) = line.strip_prefix(trimmed_for_parse) {
                    out.push_str(stripped);
                }
            }
            // Non-object JSON (a string, number, array) or a parse
            // error: pass through verbatim. The §3.4 invariant is
            // "one object per line"; anything else is malformed and
            // shouldn't survive into the prompt anyway, but better to
            // leak one bad line than to drop the entire transcript.
            _ => out.push_str(line),
        }
    }
    out
}

/// Map a 0-based speaker index to its Excel-style pseudonym:
/// 0 → "Speaker A", 25 → "Speaker Z", 26 → "Speaker AA",
/// 27 → "Speaker AB", … Pulled out of [`strip_speaker_names`] so the
/// >26-speaker contract is testable in isolation.
fn pseudonym_for_index(index: usize) -> String {
    let mut letters = String::new();
    let mut n = index;
    loop {
        let rem = n % 26;
        letters.insert(0, char::from(b'A' + (rem as u8)));
        n /= 26;
        if n == 0 {
            break;
        }
        // Excel-style: subtract 1 each iteration past the first so
        // the carry is base-26-without-zero (A=0 in the LSD; A=1 in
        // higher digits, hence "AA" follows "Z" instead of "BA").
        n -= 1;
    }
    format!("Speaker {letters}")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn small_jsonl_passes_through() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(f, r#"{{"text":"hi"}}"#).expect("w1");
        writeln!(f, r#"{{"text":"there"}}"#).expect("w2");
        let body = read_transcript_capped(&path).expect("read");
        assert!(body.contains(r#""text":"hi""#));
        assert!(body.contains(r#""text":"there""#));
    }

    #[test]
    fn rejects_oversize_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, vec![b'x'; (MAX_TRANSCRIPT_BYTES + 1024) as usize]).expect("write");
        let err = read_transcript_capped(&path).expect_err("over-cap");
        assert!(matches!(err, LlmError::Backend(s) if s.contains("exceeds")));
    }

    #[test]
    fn drops_oversize_lines_keeps_following() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&vec![b'a'; (MAX_TRANSCRIPT_LINE_BYTES + 100) as usize])
            .expect("oversize");
        f.write_all(b"\nkeeper line\n").expect("nl + keeper");
        let body = read_transcript_capped(&path).expect("read");
        assert!(body.contains("keeper line"));
        assert!(!body.contains(&"a".repeat(MAX_TRANSCRIPT_LINE_BYTES as usize)));
    }

    #[test]
    fn strip_speaker_names_assigns_letters_in_first_appearance_order() {
        // Alice → A, Bob → B, Carol → C, then Alice → A again on her
        // second turn so a re-summarize over the same transcript stays
        // stable across runs (key contract for the layer-2 ID matcher
        // not to wobble when names are pseudonymized).
        let input = concat!(
            r#"{"speaker":"Alice","text":"hi"}"#,
            "\n",
            r#"{"speaker":"Bob","text":"hello"}"#,
            "\n",
            r#"{"speaker":"Carol","text":"hey"}"#,
            "\n",
            r#"{"speaker":"Alice","text":"again"}"#,
            "\n",
        );
        let out = strip_speaker_names(input);
        assert!(out.contains(r#""speaker":"Speaker A""#), "Alice → A: {out}");
        assert!(out.contains(r#""speaker":"Speaker B""#), "Bob → B: {out}");
        assert!(out.contains(r#""speaker":"Speaker C""#), "Carol → C: {out}");
        // Alice's second turn must reuse "Speaker A" — pinning that
        // re-summarize doesn't reshuffle pseudonyms when the same
        // transcript is replayed.
        assert!(
            !out.contains("Alice"),
            "real names must not leak through to the LLM input: {out}"
        );
        assert_eq!(
            out.matches(r#""speaker":"Speaker A""#).count(),
            2,
            "Alice's two turns should share `Speaker A`: {out}"
        );
    }

    #[test]
    fn strip_speaker_names_preserves_self_and_them_sentinels() {
        // heron's writer uses `me` for the user's mic channel and
        // `them` for unattributed channel turns. The §11.2 prompt
        // template counts on those as semantic markers; pseudonymizing
        // them would conflate the user with a remote speaker.
        let input = concat!(
            r#"{"speaker":"me","text":"my line"}"#,
            "\n",
            r#"{"speaker":"them","text":"their line"}"#,
            "\n",
            r#"{"speaker":"Alice","text":"named"}"#,
            "\n",
        );
        let out = strip_speaker_names(input);
        assert!(out.contains(r#""speaker":"me""#), "me preserved: {out}");
        assert!(out.contains(r#""speaker":"them""#), "them preserved: {out}");
        assert!(out.contains(r#""speaker":"Speaker A""#), "Alice → A: {out}");
        assert!(!out.contains("Alice"), "Alice replaced: {out}");
    }

    #[test]
    fn strip_speaker_names_is_idempotent() {
        // Running the strip twice over an already-stripped transcript
        // must produce the same output. Pseudonyms are not real names,
        // so they pass through unchanged on the second pass — they
        // just claim their own pseudonym slots, which is fine because
        // the pseudonym-to-pseudonym mapping is the identity in the
        // first appearance.
        let input = concat!(
            r#"{"speaker":"Alice","text":"hi"}"#,
            "\n",
            r#"{"speaker":"Bob","text":"hello"}"#,
            "\n",
        );
        let once = strip_speaker_names(input);
        let twice = strip_speaker_names(&once);
        assert_eq!(once, twice, "strip must be idempotent");
    }

    #[test]
    fn strip_speaker_names_excel_style_after_letter_z() {
        // 27 unique speakers: indices 0..26 → A..Z, index 26 → AA.
        // Pin the >26 contract here so a future refactor that reverts
        // to numbered or wraps with `Z+1` fails loudly.
        let mut input = String::new();
        for i in 0..27 {
            input.push_str(&format!(r#"{{"speaker":"name_{i:02}","text":"x"}}"#));
            input.push('\n');
        }
        let out = strip_speaker_names(&input);
        assert!(out.contains(r#""speaker":"Speaker A""#), "A: {out}");
        assert!(out.contains(r#""speaker":"Speaker Z""#), "Z: {out}");
        assert!(
            out.contains(r#""speaker":"Speaker AA""#),
            "27th speaker → AA: {out}"
        );
        for i in 0..27 {
            let real = format!("name_{i:02}");
            assert!(!out.contains(&real), "real name `{real}` leaked: {out}");
        }
    }

    #[test]
    fn strip_speaker_names_passes_through_malformed_lines() {
        // A non-JSON line must survive: the §3.4 invariant is "one
        // object per line"; anything else is malformed but shouldn't
        // poison the whole prompt — better to leak one bad line than
        // to drop the transcript.
        let input = concat!(
            "not even json\n",
            r#"{"speaker":"Alice","text":"hi"}"#,
            "\n",
        );
        let out = strip_speaker_names(input);
        assert!(out.contains("not even json"), "malformed line preserved");
        assert!(out.contains(r#""speaker":"Speaker A""#), "Alice → A");
    }

    #[test]
    fn strip_speaker_names_passes_through_lines_without_speaker_field() {
        // A JSON object that's missing `speaker` (or where it's not a
        // string) shouldn't be mangled — pass it through unchanged.
        let input = concat!(
            r#"{"text":"no speaker"}"#,
            "\n",
            r#"{"speaker":42,"text":"non-string speaker"}"#,
            "\n",
        );
        let out = strip_speaker_names(input);
        assert!(out.contains(r#""text":"no speaker""#));
        assert!(out.contains(r#""speaker":42"#));
    }

    /// When no real name needs replacing (every speaker is a sentinel
    /// or absent), the output must be byte-identical to the input —
    /// re-serializing through serde_json would re-order keys and
    /// silently change the bytes the LLM sees, which is a hidden
    /// regression risk for the strip-names contract.
    #[test]
    fn strip_speaker_names_is_byte_identical_when_no_real_names_present() {
        // Use a key order serde_json's default BTreeMap-backed Map
        // would shuffle if it re-serialized: `t0` < `text` < `speaker`
        // alphabetically; the input puts `t0` after `speaker`, which
        // a BTreeMap re-serialize would silently reorder.
        let input = concat!(
            r#"{"speaker":"me","t0":0,"text":"hi"}"#,
            "\n",
            r#"{"speaker":"them","t0":1,"text":"there"}"#,
            "\n",
            r#"{"text":"no speaker field","t0":2}"#,
            "\n",
        );
        let out = strip_speaker_names(input);
        assert_eq!(
            out, input,
            "no real-name lines must pass through byte-identically"
        );
    }

    #[test]
    fn build_user_content_separates_prompt_from_transcript() {
        let out = build_user_content("PROMPT", "TRANSCRIPT");
        assert!(out.starts_with("PROMPT"));
        assert!(out.contains("Transcript JSONL"));
        assert!(out.contains("TRANSCRIPT"));
    }
}
