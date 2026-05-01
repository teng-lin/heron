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

/// Scan `invoke.ts` for the `HeronCommands` interface block and
/// collect every property key that starts with `heron_`. We don't
/// have a TS parser on hand; the line-based scanner is pinned to two
/// fragile-but-stable anchors (`export interface HeronCommands {` /
/// the matching `}`) and rejects shapes we haven't seen before so
/// future drift in the surrounding file fails loudly rather than
/// silently dropping commands.
fn ts_command_names() -> BTreeSet<String> {
    let path = {
        // `CARGO_MANIFEST_DIR` is `apps/desktop/src-tauri`; walk up
        // one directory to reach `apps/desktop/src/lib/invoke.ts`.
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.push("src/lib/invoke.ts");
        p
    };
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    const OPEN: &str = "export interface HeronCommands {";
    let open_idx = src
        .find(OPEN)
        .unwrap_or_else(|| panic!("`{OPEN}` anchor not found in {}", path.display()));
    let after_open = &src[open_idx + OPEN.len()..];

    // Brace-balanced scan for the matching close. The interface body
    // contains `{}` literals (e.g. `args: Record<string, never>`) so a
    // naive `find('}')` would stop at the first one.
    let mut depth = 1usize;
    let mut close_rel: Option<usize> = None;
    for (i, ch) in after_open.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
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
    for raw in body.lines() {
        // The interface body has only one shape per command:
        //   <key>: {
        // where `<key>` is unquoted and the colon is followed by an
        // open brace. We explicitly skip quoted keys / multi-line
        // arrow types so a future syntax change can't silently widen
        // the matched set.
        let line = raw.trim();
        if !line.ends_with(": {") {
            continue;
        }
        let key = line.trim_end_matches(": {").trim();
        // Reject quoted keys — a future `"heron_foo": {` would be an
        // unintentional widening of the matched set.
        if key.starts_with('"') || key.starts_with('\'') {
            panic!(
                "invoke.ts uses a quoted property key in HeronCommands; \
                 update the IPC contract test to handle it: {raw:?}"
            );
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
