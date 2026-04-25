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
    fn build_user_content_separates_prompt_from_transcript() {
        let out = build_user_content("PROMPT", "TRANSCRIPT");
        assert!(out.starts_with("PROMPT"));
        assert!(out.contains("Transcript JSONL"));
        assert!(out.contains("TRANSCRIPT"));
    }
}
