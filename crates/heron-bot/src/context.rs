//! Pre-meeting context rendering per spec §6 + §8 + Invariant 10.
//!
//! [`crate::PreMeetingContext`] carries everything the agent needs
//! to know about a meeting before it joins: the agenda, who's
//! coming, prior-meeting context, the user's pre-call briefing.
//! [`render`] turns that struct into a single system-prompt string
//! that `heron-realtime`'s `SessionConfig.system_prompt` consumes.
//!
//! Spec **Invariant 10**: the system prompt must stay under a 16K
//! token budget. We don't ship a tokenizer, so we use a
//! 3-bytes-per-token heuristic = 48 KiB cap. OpenAI's o200k_base
//! averages ~3.3 bytes/token for English prose and ~2 bytes/token
//! for dense JSON; the persona prompt + tool schemas the realtime
//! backend appends are exactly the dense-JSON case, so erring
//! conservative leaves real headroom rather than the optimistic 4-
//! bytes-per-token estimate.
//!
//! **Free-form fields are not sanitized.** `agenda` and
//! `user_briefing` are emitted verbatim into the prompt; if the
//! caller's text already contains a `## ` header, it'll fool a
//! downstream parser that uses section-counting. The orchestrator
//! is the right layer to strip / quote these strings.
//!
//! ## Render shape
//!
//! ```text
//! ## Agenda
//! <agenda>
//!
//! ## Attendees
//! - <name> (<company-or-relationship>): <notes>
//! - …
//!
//! ## Related notes (paths)
//! - <vault-path>
//! - …
//!
//! ## Briefing
//! <briefing>
//! ```
//!
//! Empty sections are omitted entirely. A `PreMeetingContext` with
//! zero fields populated renders to an empty string — the caller
//! decides whether to substitute a default persona prompt.

use crate::{AttendeeContext, PreMeetingContext};

/// Heuristic budget. 16K tokens × 3 bytes/token = 48 KiB. Erring
/// conservative because the caller's persona prompt + tool schemas
/// (dense JSON, ~2 bytes/token in practice) eat into the same 16K
/// real-tokenizer budget; a 4-bytes/token estimate would leave us
/// over-budget in the common case.
pub const MAX_CONTEXT_BYTES: usize = 48 * 1024;

/// Errors [`render`] can return.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContextError {
    #[error(
        "rendered context is {bytes} bytes, exceeds {max}-byte budget; \
         summarize agenda / briefing or drop attendee notes",
        max = MAX_CONTEXT_BYTES,
    )]
    TooLarge { bytes: usize },
}

/// Render `ctx` into a system-prompt-ready string. Returns `Ok("")`
/// for an empty context (caller decides whether to substitute a
/// persona default).
pub fn render(ctx: &PreMeetingContext) -> Result<String, ContextError> {
    let mut out = String::new();
    let mut needs_blank_line = false;

    if let Some(agenda) = ctx.agenda.as_ref() {
        let trimmed = agenda.trim();
        if !trimmed.is_empty() {
            out.push_str("## Agenda\n");
            out.push_str(trimmed);
            out.push('\n');
            needs_blank_line = true;
        }
    }

    // Defer the section header until we see at least one renderable
    // attendee. A vec full of whitespace-only names would otherwise
    // produce a bare "## Attendees" with no body — contradicting the
    // module's "empty sections are omitted entirely" contract.
    let mut attendees_rendered = false;
    for att in &ctx.attendees_known {
        if att.name.trim().is_empty() {
            continue;
        }
        if !attendees_rendered {
            if needs_blank_line {
                out.push('\n');
            }
            out.push_str("## Attendees\n");
            attendees_rendered = true;
        }
        render_attendee(&mut out, att);
    }
    if attendees_rendered {
        needs_blank_line = true;
    }

    // Same deferred-header pattern: a vec of whitespace-only paths
    // shouldn't emit a stranded "## Related notes (paths)" header.
    let mut notes_rendered = false;
    for note in &ctx.related_notes {
        let trimmed = note.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !notes_rendered {
            if needs_blank_line {
                out.push('\n');
            }
            out.push_str("## Related notes (paths)\n");
            notes_rendered = true;
        }
        out.push_str("- ");
        out.push_str(trimmed);
        out.push('\n');
    }
    if notes_rendered {
        needs_blank_line = true;
    }

    if let Some(briefing) = ctx.user_briefing.as_ref() {
        let trimmed = briefing.trim();
        if !trimmed.is_empty() {
            if needs_blank_line {
                out.push('\n');
            }
            out.push_str("## Briefing\n");
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    if out.len() > MAX_CONTEXT_BYTES {
        return Err(ContextError::TooLarge { bytes: out.len() });
    }
    Ok(out)
}

fn render_attendee(out: &mut String, att: &AttendeeContext) {
    out.push_str("- ");
    out.push_str(att.name.trim());

    // "(relationship)" — when set. Email domain → company is a
    // future feature; today we don't extract company from email
    // because email is intentionally absent from the rendered
    // prompt (see the `attendee_email_is_not_in_render_today` test).
    let tag = att
        .relationship
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(tag) = tag {
        out.push_str(" (");
        out.push_str(tag);
        out.push(')');
    }

    if let Some(notes) = att.notes.as_deref() {
        let trimmed = notes.trim();
        if !trimmed.is_empty() {
            out.push_str(": ");
            out.push_str(trimmed);
        }
    }
    out.push('\n');
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn ctx() -> PreMeetingContext {
        PreMeetingContext::default()
    }

    fn attendee(name: &str) -> AttendeeContext {
        AttendeeContext {
            name: name.to_owned(),
            email: None,
            last_seen_in: None,
            relationship: None,
            notes: None,
        }
    }

    #[test]
    fn empty_context_renders_to_empty_string() {
        assert_eq!(render(&ctx()).expect("ok"), "");
    }

    #[test]
    fn agenda_only_renders_with_header() {
        let mut c = ctx();
        c.agenda = Some("Discuss Q3 targets".into());
        let out = render(&c).expect("ok");
        assert_eq!(out, "## Agenda\nDiscuss Q3 targets\n");
    }

    #[test]
    fn agenda_whitespace_only_is_omitted() {
        let mut c = ctx();
        c.agenda = Some("   \n\t  ".into());
        assert_eq!(render(&c).expect("ok"), "");
    }

    #[test]
    fn agenda_is_trimmed() {
        let mut c = ctx();
        c.agenda = Some("  Q3 targets  \n".into());
        let out = render(&c).expect("ok");
        assert_eq!(out, "## Agenda\nQ3 targets\n");
    }

    #[test]
    fn attendees_render_as_bulleted_list() {
        let mut c = ctx();
        c.attendees_known = vec![attendee("Alice"), attendee("Bob")];
        let out = render(&c).expect("ok");
        assert!(out.contains("## Attendees\n"));
        assert!(out.contains("- Alice\n"));
        assert!(out.contains("- Bob\n"));
    }

    #[test]
    fn attendee_with_relationship_and_notes() {
        let mut c = ctx();
        c.attendees_known = vec![AttendeeContext {
            name: "Alice".into(),
            email: None,
            last_seen_in: None,
            relationship: Some("CFO at Acme".into()),
            notes: Some("prefers concise updates".into()),
        }];
        let out = render(&c).expect("ok");
        assert!(out.contains("- Alice (CFO at Acme): prefers concise updates\n"));
    }

    #[test]
    fn attendee_with_only_relationship_omits_notes() {
        let mut c = ctx();
        c.attendees_known = vec![AttendeeContext {
            name: "Alice".into(),
            email: None,
            last_seen_in: None,
            relationship: Some("CFO".into()),
            notes: None,
        }];
        let out = render(&c).expect("ok");
        assert!(out.contains("- Alice (CFO)\n"));
        assert!(!out.contains("(CFO):"));
    }

    #[test]
    fn attendee_email_is_not_in_render_today() {
        // Email is captured at the type level but the spec doesn't
        // ask for it in the system prompt — the persona doesn't
        // address attendees by email. Pin so a future addition is
        // an explicit decision rather than a leak.
        let mut c = ctx();
        c.attendees_known = vec![AttendeeContext {
            name: "Alice".into(),
            email: Some("alice@acme.example".into()),
            last_seen_in: None,
            relationship: None,
            notes: None,
        }];
        let out = render(&c).expect("ok");
        assert!(!out.contains("alice@acme.example"));
    }

    #[test]
    fn related_notes_render_as_paths_only() {
        let mut c = ctx();
        c.related_notes = vec![
            "vault/2026-04-22-acme-q3.md".into(),
            "vault/2026-04-15-acme-roadmap.md".into(),
        ];
        let out = render(&c).expect("ok");
        assert!(out.contains("## Related notes (paths)\n"));
        assert!(out.contains("- vault/2026-04-22-acme-q3.md\n"));
    }

    #[test]
    fn attendees_all_whitespace_names_omit_section() {
        // Vec is non-empty but every name trims to empty → header
        // would otherwise be stranded with bare `- ` bullets.
        let mut c = ctx();
        c.attendees_known = vec![attendee(""), attendee("   "), attendee("\t\n")];
        let out = render(&c).expect("ok");
        assert_eq!(out, "", "expected fully empty render, got: {out:?}");
    }

    #[test]
    fn attendees_section_only_includes_non_empty_names() {
        // Mixed: one real attendee + whitespace-only padding. Header
        // must appear once, only the real attendee bulleted.
        let mut c = ctx();
        c.attendees_known = vec![attendee("   "), attendee("Alice"), attendee("")];
        let out = render(&c).expect("ok");
        assert_eq!(out, "## Attendees\n- Alice\n", "got: {out:?}");
    }

    #[test]
    fn related_notes_all_whitespace_omit_section() {
        // All whitespace-only paths → entire section skipped, no
        // stranded "## Related notes (paths)" header.
        let mut c = ctx();
        c.related_notes = vec!["".into(), "   ".into(), "\n\t".into()];
        let out = render(&c).expect("ok");
        assert_eq!(out, "", "expected fully empty render, got: {out:?}");
    }

    #[test]
    fn empty_attendees_between_filled_sections_keeps_clean_separators() {
        // Regression for the deferred-header rewrite: a whitespace-
        // only attendees vec sandwiched between agenda and briefing
        // must not introduce a double blank line.
        let mut c = ctx();
        c.agenda = Some("Q3".into());
        c.attendees_known = vec![attendee(""), attendee("  ")];
        c.user_briefing = Some("Be concise".into());
        let out = render(&c).expect("ok");
        assert_eq!(
            out, "## Agenda\nQ3\n\n## Briefing\nBe concise\n",
            "got: {out:?}"
        );
    }

    #[test]
    fn related_notes_skips_whitespace_only_entries() {
        let mut c = ctx();
        c.related_notes = vec!["".into(), "   ".into(), "real-path.md".into()];
        let out = render(&c).expect("ok");
        assert!(out.contains("- real-path.md\n"));
        // The empty / whitespace-only entries shouldn't appear as bare hyphens.
        assert!(!out.contains("- \n"));
    }

    #[test]
    fn briefing_renders_with_header() {
        let mut c = ctx();
        c.user_briefing = Some("They're nervous about pricing".into());
        let out = render(&c).expect("ok");
        assert!(out.contains("## Briefing\nThey're nervous about pricing\n"));
    }

    #[test]
    fn full_context_renders_with_blank_line_separators() {
        let mut c = ctx();
        c.agenda = Some("Q3 review".into());
        c.attendees_known = vec![attendee("Alice")];
        c.related_notes = vec!["vault/note.md".into()];
        c.user_briefing = Some("Be concise".into());
        let out = render(&c).expect("ok");

        // Verify section ordering + separators.
        let sections: Vec<&str> = out.split("\n\n").collect();
        assert_eq!(sections.len(), 4, "got {sections:?}");
        assert!(sections[0].starts_with("## Agenda"));
        assert!(sections[1].starts_with("## Attendees"));
        assert!(sections[2].starts_with("## Related notes"));
        assert!(sections[3].starts_with("## Briefing"));
    }

    #[test]
    fn skipping_middle_section_keeps_separators_clean() {
        // Agenda + briefing only (no attendees, no notes) — should
        // still render with one blank line between, not two.
        let mut c = ctx();
        c.agenda = Some("Q3 review".into());
        c.user_briefing = Some("Be concise".into());
        let out = render(&c).expect("ok");
        assert_eq!(
            out, "## Agenda\nQ3 review\n\n## Briefing\nBe concise\n",
            "got: {out:?}"
        );
    }

    #[test]
    fn over_budget_returns_too_large_error() {
        let mut c = ctx();
        // 64 KiB of agenda → over the 60 KiB cap.
        c.agenda = Some("x".repeat(MAX_CONTEXT_BYTES + 1024));
        let err = render(&c).expect_err("over-cap");
        match err {
            ContextError::TooLarge { bytes } => {
                assert!(bytes > MAX_CONTEXT_BYTES, "got {bytes}");
            }
        }
    }

    #[test]
    fn just_under_budget_succeeds() {
        let mut c = ctx();
        // "## Agenda\n" header is 10 bytes; trailing "\n" after the
        // payload is 1 byte. Subtract 11 + a 1-byte slack so we're
        // sitting one byte under the cap rather than exactly on it
        // (a fuzzy off-by-one in either direction wouldn't surface
        // a real bug — pin "just under" not "exactly at").
        let payload_size = MAX_CONTEXT_BYTES - 12;
        c.agenda = Some("x".repeat(payload_size));
        let result = render(&c);
        assert!(result.is_ok(), "expected Ok at boundary: {result:?}");
    }

    #[test]
    fn rendered_output_is_deterministic_for_same_input() {
        // Pin determinism so the same context renders to the same
        // bytes across runs — the diagnostics tab + audit log
        // depend on this.
        let mut c = ctx();
        c.agenda = Some("Q3".into());
        c.attendees_known = vec![attendee("Alice"), attendee("Bob")];
        c.related_notes = vec!["a.md".into(), "b.md".into()];
        c.user_briefing = Some("Brief".into());
        let first = render(&c).expect("ok");
        let second = render(&c).expect("ok");
        assert_eq!(first, second);
    }
}
