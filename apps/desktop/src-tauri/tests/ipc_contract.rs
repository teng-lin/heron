//! IPC contract test: every `tauri::generate_handler!` entry in
//! `apps/desktop/src-tauri/src/lib.rs` must have a matching key in
//! `apps/desktop/src/lib/invoke.ts::HeronCommands`, and vice versa.
//!
//! Origin: issue #185. The test that lives here is the canonical
//! anti-drift gate for the Rust ↔ TS IPC surface. Adding a Rust
//! command without a TS binding (or removing a TS binding without
//! removing the Rust handler) fails this test in the `rust.yml`
//! workflow before the change can merge. Cross-workflow handoff is
//! avoided by reading both source files directly: the Rust side is
//! parsed via `syn`, the TS side via a small line-based scanner
//! pinned to the literal `HeronCommands` interface block.
//!
//! When the parity test fails the assertion message lists both
//! diff directions (`only_in_rust` / `only_in_ts`) so the fix is
//! one of: rename the handler, add the binding, or delete the dead
//! entry on whichever side is stale.

use std::collections::BTreeSet;
use std::path::PathBuf;

use syn::{ExprMacro, Item, visit::Visit};

/// Resolve a path under `apps/desktop/` from `CARGO_MANIFEST_DIR`
/// (= `apps/desktop/src-tauri`). Cargo guarantees this env var at
/// test build time; the test binary is invoked from the crate root.
fn manifest_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push(rel);
    p
}

/// Walk the parsed `lib.rs` AST for the `tauri::generate_handler![…]`
/// macro invocation inside `pub fn run`. Each comma-separated entry
/// is a path (`heron_status` or `meetings::heron_get_meeting`); the
/// last segment is the wire-format command name the renderer's
/// `invoke()` call site uses.
#[derive(Default)]
struct HandlerVisitor {
    names: BTreeSet<String>,
    saw_macro: bool,
}

impl<'ast> Visit<'ast> for HandlerVisitor {
    fn visit_expr_macro(&mut self, mac: &'ast ExprMacro) {
        // The macro is invoked as `tauri::generate_handler!`. The
        // path's last segment is the name we match on so the test
        // doesn't break if a future `use tauri::generate_handler`
        // shortens the call site.
        let last = mac
            .mac
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        if last != "generate_handler" {
            syn::visit::visit_expr_macro(self, mac);
            return;
        }
        self.saw_macro = true;
        // The macro accepts a bracketed array of paths. We parse the
        // tokens as `Punctuated<Path, Token![,]>` and pull the last
        // path segment off each one.
        let parsed: syn::Result<syn::punctuated::Punctuated<syn::Path, syn::Token![,]>> = mac
            .mac
            .parse_body_with(syn::punctuated::Punctuated::parse_terminated);
        let paths = parsed.unwrap_or_else(|e| {
            panic!("failed to parse tauri::generate_handler! body: {e}");
        });
        for path in paths {
            let segment = path
                .segments
                .last()
                .unwrap_or_else(|| panic!("empty path in generate_handler!"))
                .ident
                .to_string();
            self.names.insert(segment);
        }
    }
}

fn rust_handler_names() -> BTreeSet<String> {
    let path = manifest_path("src/lib.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let file = syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let mut visitor = HandlerVisitor::default();
    // Walk the whole file rather than narrowing to `pub fn run` so a
    // future split into multiple handler blocks doesn't silently
    // shrink the assertion set.
    for item in &file.items {
        if let Item::Fn(_) = item {
            visitor.visit_item(item);
        }
    }
    assert!(
        visitor.saw_macro,
        "no tauri::generate_handler! invocation found in {}",
        path.display()
    );
    assert!(
        !visitor.names.is_empty(),
        "tauri::generate_handler! parsed to zero handlers in {}",
        path.display()
    );
    visitor.names
}

/// Tiny TS lexer that yields one byte at a time with metadata about
/// whether the byte is "structural" (matters for brace counting and
/// key extraction) or part of a comment / string literal that should
/// be ignored. Operates on raw bytes — TS source we control here is
/// ASCII for everything we care about (`{`, `}`, `"`, `'`, `` ` ``,
/// `/`, `*`, `\n`); UTF-8 multi-byte characters can appear inside
/// comments or strings and are correctly subsumed by the existing
/// "in_comment / in_string" state because the lexer never emits
/// those bytes as structural.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LexState {
    Code,
    LineComment,
    BlockComment,
    DoubleString,
    SingleString,
    TemplateString,
}

/// Strip TypeScript comments + string literals from `src`, returning
/// a buffer the same length where each non-structural byte has been
/// replaced with a space (so byte indices into the result are still
/// valid offsets into the original — useful for error messages, and
/// keeps the line numbers stable since `\n` is preserved). Templates
/// can contain `${ … }` expressions that re-enter code mode; we
/// handle one level of nesting which is sufficient for the
/// `HeronCommands` block (no template literals appear there today;
/// the handler is belt-and-suspenders).
fn strip_ts_strings_and_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let mut state = LexState::Code;
    // Stack of `Code` re-entries from `${` inside a template string.
    // Each entry remembers which template state to return to when the
    // matching `}` closes the expression.
    let mut template_stack: Vec<LexState> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // `\n` is preserved verbatim across all states so line-based
        // splits downstream still report the original line numbers.
        if b == b'\n' {
            out[i] = b'\n';
            if matches!(state, LexState::LineComment) {
                state = LexState::Code;
            }
            i += 1;
            continue;
        }
        match state {
            LexState::Code => {
                // Handle `//` and `/*` comment openers.
                if b == b'/' && i + 1 < bytes.len() {
                    let next = bytes[i + 1];
                    if next == b'/' {
                        state = LexState::LineComment;
                        i += 2;
                        continue;
                    }
                    if next == b'*' {
                        state = LexState::BlockComment;
                        i += 2;
                        continue;
                    }
                }
                // String openers.
                if b == b'"' {
                    state = LexState::DoubleString;
                    i += 1;
                    continue;
                }
                if b == b'\'' {
                    state = LexState::SingleString;
                    i += 1;
                    continue;
                }
                if b == b'`' {
                    state = LexState::TemplateString;
                    i += 1;
                    continue;
                }
                // Pop a template expression on `}` if we entered one.
                if b == b'}'
                    && let Some(prev) = template_stack.pop()
                {
                    // The closing brace of `${ … }` is structural for
                    // the *expression*, but we treat the brace itself
                    // as part of the string so it doesn't confuse the
                    // outer brace counter.
                    state = prev;
                    i += 1;
                    continue;
                }
                // Otherwise this is structural code.
                out[i] = b;
                i += 1;
            }
            LexState::LineComment => {
                // Already handled `\n` above; everything else is
                // suppressed to a space.
                i += 1;
            }
            LexState::BlockComment => {
                if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    state = LexState::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            LexState::DoubleString | LexState::SingleString => {
                // `\` escapes the next byte; never closes the string.
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                let close = match state {
                    LexState::DoubleString => b'"',
                    LexState::SingleString => b'\'',
                    _ => unreachable!(),
                };
                if b == close {
                    state = LexState::Code;
                }
                i += 1;
            }
            LexState::TemplateString => {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'`' {
                    state = LexState::Code;
                    i += 1;
                    continue;
                }
                if b == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    // Re-enter Code mode, remember to come back here.
                    template_stack.push(LexState::TemplateString);
                    state = LexState::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
    }
    // The lexer should always terminate inside `Code` for a well-formed
    // TS file. A trailing un-closed string / comment would be a
    // pre-existing TS-compile error caught upstream by `bun run build`.
    // Convert `out` back to a `String`; all bytes are either preserved
    // structural ASCII or `b' '` / `b'\n'`, so it's valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|e| panic!("stripped buffer must be valid UTF-8: {e}"))
}

/// Scan `invoke.ts` for the `HeronCommands` interface block and
/// collect every property key that starts with `heron_`. We don't
/// have a TS parser on hand; the byte-level scanner pre-strips
/// comments + string literals (so a doc-comment containing `}` or
/// a string with `: {` cannot fool the brace counter or the
/// per-line key matcher) and then locates the literal anchor
/// `export interface HeronCommands {` inside the *stripped* buffer
/// — this prevents an anchor inside a comment or string from being
/// mistaken for the real interface declaration.
fn ts_command_names() -> BTreeSet<String> {
    let path = {
        // `CARGO_MANIFEST_DIR` is `apps/desktop/src-tauri`; walk up
        // one directory to reach `apps/desktop/src/lib/invoke.ts`.
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.push("src/lib/invoke.ts");
        p
    };
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let src = strip_ts_strings_and_comments(&raw);

    const OPEN: &str = "export interface HeronCommands {";
    let open_idx = src
        .find(OPEN)
        .unwrap_or_else(|| panic!("`{OPEN}` anchor not found in {}", path.display()));
    // Sanity-check we found it once and only once. A duplicate hit
    // would mean either a copy-paste error or our anchor is too
    // generic — either case wants a hard failure.
    let after_first = &src[open_idx + OPEN.len()..];
    if after_first.contains(OPEN) {
        panic!("`{OPEN}` appears more than once in {}", path.display());
    }
    let after_open = after_first;

    // Brace-balanced scan for the matching close, run on the stripped
    // buffer so braces inside comments / strings can't move the depth.
    let mut depth = 1usize;
    let mut close_rel: Option<usize> = None;
    for (i, &b) in after_open.as_bytes().iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close_rel = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close_rel =
        close_rel.unwrap_or_else(|| panic!("unbalanced braces after `{OPEN}` in invoke.ts"));
    let body = &after_open[..close_rel];

    let mut out = BTreeSet::new();
    for raw_line in body.lines() {
        // Per-line parse against a simple grammar: optional leading
        // whitespace, then an identifier (the key), then `:`, then
        // `{`, then optional trailing whitespace. This is more
        // tolerant of the line-ending `: {` shape than the previous
        // `ends_with(": {")` check (e.g. it accepts `: {  ` or
        // `:  {`) without widening to multi-key lines like
        // `args: { sessionId: string };` — those are indented further
        // and start with `args:` (or some other reserved prefix), and
        // our `heron_` start filter still catches the actual keys.
        let trimmed = raw_line.trim_start();
        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let after_colon = rest.trim_start();
        // After the colon, the next non-whitespace char must be `{`
        // and the rest of the line (modulo whitespace) must be empty.
        // If it's anything else (`Record<…>`, `string;`, `(args:…)`)
        // we're not looking at a top-level command entry.
        if !after_colon.starts_with('{') {
            continue;
        }
        let tail = after_colon[1..].trim();
        if !tail.is_empty() {
            continue;
        }
        if !key.starts_with("heron_") {
            continue;
        }
        // Reject identifiers we can't trust as plain ASCII snake_case;
        // matches what Rust accepts for `#[tauri::command]` fn names.
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            panic!("unexpected character in HeronCommands key {key:?}");
        }
        // Duplicate keys are a TS compile error already, but the set
        // insertion silently dedupes — surface the dup so a copy-paste
        // mistake during a rebase fails this test rather than the
        // downstream parity check (whose message would be misleading).
        if !out.insert(key.to_owned()) {
            panic!("duplicate HeronCommands key {key:?} in invoke.ts");
        }
    }
    assert!(
        !out.is_empty(),
        "no `heron_*` keys found in HeronCommands block of {}",
        path.display()
    );
    out
}

/// Format the diff between the two sets and panic with the
/// human-readable message a contributor needs to fix the drift.
#[track_caller]
fn assert_set_eq(rust: &BTreeSet<String>, ts: &BTreeSet<String>) {
    if rust == ts {
        return;
    }
    let only_in_rust: Vec<&String> = rust.difference(ts).collect();
    let only_in_ts: Vec<&String> = ts.difference(rust).collect();
    panic!(
        "IPC command-name drift between Rust and TS:\n  \
         only in Rust (add to invoke.ts::HeronCommands): {only_in_rust:?}\n  \
         only in TS   (add #[tauri::command] + register, or remove from invoke.ts): {only_in_ts:?}\n\
         Rust handlers ({} total): {rust:?}\nTS commands ({} total): {ts:?}",
        rust.len(),
        ts.len(),
    );
}

#[test]
fn ipc_command_names_match_between_rust_and_ts() {
    let rust = rust_handler_names();
    let ts = ts_command_names();
    assert_set_eq(&rust, &ts);
}

/// Anti-trivial-pass: the test fails loudly if either side parsed to
/// an empty set (the helper functions panic on empty already, but
/// belt-and-suspenders here so a future refactor that swallows the
/// panic still surfaces). Pinned at >= 50 so the assertion bites
/// before the surface shrinks accidentally — today's count is 59.
#[test]
fn ipc_contract_test_is_not_trivially_passing() {
    let rust = rust_handler_names();
    let ts = ts_command_names();
    assert!(
        rust.len() >= 50,
        "Rust handler count fell below 50 ({}); did the IPC surface shrink unexpectedly?",
        rust.len()
    );
    assert!(
        ts.len() >= 50,
        "TS command count fell below 50 ({}); did invoke.ts get split?",
        ts.len()
    );
}

#[cfg(test)]
mod ts_lexer_tests {
    //! Targeted unit tests for `strip_ts_strings_and_comments` so a
    //! regression in the helper doesn't have to be discovered via the
    //! full parity check.
    use super::strip_ts_strings_and_comments;

    /// `}` inside a string literal must NOT count as structural.
    #[test]
    fn brace_inside_double_string_is_suppressed() {
        let stripped = strip_ts_strings_and_comments(r#"a = "}"; b = {};"#);
        // Strings collapse to spaces; the real `{}` survives.
        assert!(stripped.contains('{'));
        // Only one `}` (the structural one); the one inside the
        // string was suppressed.
        assert_eq!(stripped.matches('}').count(), 1);
        assert_eq!(stripped.matches('{').count(), 1);
    }

    /// `}` inside `'…'` must NOT count as structural.
    #[test]
    fn brace_inside_single_string_is_suppressed() {
        let stripped = strip_ts_strings_and_comments("a = '}'; b = {};");
        assert_eq!(stripped.matches('}').count(), 1);
        assert_eq!(stripped.matches('{').count(), 1);
    }

    /// `}` inside a `// …` line comment must NOT count.
    #[test]
    fn brace_inside_line_comment_is_suppressed() {
        let stripped = strip_ts_strings_and_comments("a = 1; // }\nb = {};");
        assert_eq!(stripped.matches('}').count(), 1);
        // Line comment ends at `\n`; structural `{}` after survives.
        assert!(stripped.contains("\n"));
    }

    /// `}` inside a `/* … */` block comment must NOT count even when
    /// it spans newlines.
    #[test]
    fn brace_inside_block_comment_is_suppressed() {
        let stripped = strip_ts_strings_and_comments("a = 1; /* } \n still } */ b = {};");
        assert_eq!(stripped.matches('}').count(), 1);
        assert_eq!(stripped.matches('{').count(), 1);
    }

    /// Escape sequences inside strings must not prematurely close
    /// the string.
    #[test]
    fn escaped_quote_inside_string_does_not_close_it() {
        let stripped = strip_ts_strings_and_comments(r#"a = "x\"}"; b = {};"#);
        assert_eq!(stripped.matches('}').count(), 1);
        assert_eq!(stripped.matches('{').count(), 1);
    }

    /// Newlines are preserved verbatim across all states so line
    /// numbering survives the strip.
    #[test]
    fn newline_count_is_preserved() {
        let src = "// a\n\"x\"\n/* b\n */\n`t`\n";
        let stripped = strip_ts_strings_and_comments(src);
        assert_eq!(
            src.matches('\n').count(),
            stripped.matches('\n').count(),
            "stripped buffer must preserve every newline"
        );
        assert_eq!(
            src.len(),
            stripped.len(),
            "stripped len must equal input len"
        );
    }
}
