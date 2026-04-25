/**
 * TipTap-3 markdown editor used by the Review route.
 *
 * Mounts a single TipTap editor with `StarterKit` (paragraphs / lists
 * / headings / inline marks), `Typography` (smart quotes, em-dash,
 * etc.), `Placeholder`, and `tiptap-markdown` for round-trip
 * markdown serialization.
 *
 * The editor is uncontrolled — TipTap owns the canonical state — but
 * the parent gets a `getMarkdown()` callback via `editorRef` so it
 * can pull the latest content out for save. `onBlur` fires `onSave`
 * with the freshest markdown so a user clicking away (or using ⌘S
 * via the parent's keydown handler) always persists their edits.
 *
 * Toolbar is intentionally absent for v1: the Typography extension
 * + StarterKit's markdown shortcuts (`# heading`, `- list`,
 * `**bold**`, etc.) cover the formatting needs of a meeting note
 * without UI chrome.
 *
 * Re-mounting on session change is handled by the parent via a
 * `key={...}` prop — there's no `setMarkdown`/`useEffect` dance to
 * sync external changes; that approach was lossy (whitespace
 * normalization in tiptap-markdown's serializer never round-trips
 * exactly) and the parent always knows when content needs to swap.
 */

import { useImperativeHandle } from "react";
import { type Editor, EditorContent, useEditor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import Typography from "@tiptap/extension-typography";
import Placeholder from "@tiptap/extension-placeholder";
import { Markdown } from "tiptap-markdown";

/**
 * The `tiptap-markdown` extension installs a `markdown` field on the
 * editor's `storage` map at runtime. Its types live in the package's
 * `MarkdownStorage` interface but are not registered with TipTap's
 * generic `Storage` type — pull it out manually so the rest of this
 * file stays typesafe without `// @ts-ignore`.
 */
function getMarkdown(editor: Editor | null | undefined): string {
  if (!editor) return "";
  const storage = editor.storage as unknown as {
    markdown?: { getMarkdown(): string };
  };
  return storage.markdown?.getMarkdown() ?? "";
}

export interface NoteEditorHandle {
  /** Pull the current document as a markdown string. */
  getMarkdown(): string;
}

interface NoteEditorProps {
  /** Initial markdown — used once on mount. The parent re-mounts
   * via `key` when it needs to swap content. */
  initialMarkdown: string;
  /** Called on every edit with the latest markdown. Cheap; powers
   * the live transcript view. Does NOT save. */
  onUpdate?: (markdown: string) => void;
  /** Called when the editor loses focus. Receives the current
   * markdown string. */
  onBlurSave: (markdown: string) => void;
  /** Imperative handle so the parent can pull markdown on ⌘S. */
  ref?: React.Ref<NoteEditorHandle>;
}

export function NoteEditor({
  initialMarkdown,
  onUpdate,
  onBlurSave,
  ref,
}: NoteEditorProps) {
  const editor = useEditor({
    extensions: [
      StarterKit,
      Typography,
      Placeholder.configure({
        placeholder: "Start typing your meeting notes…",
      }),
      Markdown.configure({
        // Don't auto-link plain URLs — meeting notes often paste
        // raw join links and we don't want them mutating into
        // anchor marks.
        linkify: false,
        // Preserve the .md's hard-wrap behavior on round-trip.
        breaks: false,
        // Heron writes its `.md` with `*` bullet markers in
        // `heron-vault::encode`; mirror that so editor saves don't
        // diff against the freshly-summarized output.
        bulletListMarker: "*",
        // Don't render embedded HTML — Heron `.md`s are pure
        // markdown and accidental HTML in a transcript line should
        // not execute.
        html: false,
      }),
    ],
    content: initialMarkdown,
    editorProps: {
      attributes: {
        // `prose` from @tailwindcss/typography supplies the body
        // styling; `focus:outline-none` removes the default editor
        // outline (we already wrap in a focus ring at the page
        // level).
        class:
          "prose prose-sm max-w-none focus:outline-none min-h-[60vh] py-4",
      },
    },
    onUpdate: ({ editor: ed }) => {
      onUpdate?.(getMarkdown(ed));
    },
    onBlur: ({ editor: ed }) => {
      onBlurSave(getMarkdown(ed));
    },
    // Phase 65 ships the immutable v1 content surface. PR-γ′ may
    // toggle this off during playback; nothing to wire today.
    immediatelyRender: false,
  });

  useImperativeHandle(
    ref,
    (): NoteEditorHandle => ({
      getMarkdown: () => getMarkdown(editor),
    }),
    [editor],
  );

  if (!editor) {
    return (
      <div className="text-sm text-muted-foreground py-8 text-center">
        Loading editor…
      </div>
    );
  }

  return <EditorContent editor={editor} />;
}
