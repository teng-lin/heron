import { useEffect, useRef, useState } from "react";

import { Label } from "../../../components/ui/label";
import { useSettingsStore } from "../../../store/settings";

/**
 * Hotwords vocabulary boost. Persisted as `Vec<String>` and threaded
 * through to the WhisperKit `DecodingOptions.promptTokens` per Tier 4
 * (PR #166). One word/phrase per line; blank lines are stripped on save.
 */
export function HotwordsField() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  const storeHotwords = settings?.hotwords;
  // Local draft so the textarea owns its own per-keystroke string. The
  // store-side `Vec<String>` is normalized (split on `\n`, trimmed,
  // empties dropped) only on blur — splitting on every keystroke
  // collapses in-progress newlines / whitespace and jitters the
  // cursor while the user is mid-edit.
  const [draft, setDraft] = useState<string>(() =>
    (storeHotwords ?? []).join("\n"),
  );
  const lastSyncedRef = useRef<readonly string[] | null>(null);

  // Mirror an external store change (load, or another tab editing the
  // same field) into the draft. Reference-equal incoming values skip
  // the reset so an in-flight save doesn't clobber the user's typing.
  useEffect(() => {
    if (storeHotwords === undefined) return;
    if (storeHotwords === lastSyncedRef.current) return;
    setDraft(storeHotwords.join("\n"));
    lastSyncedRef.current = storeHotwords;
  }, [storeHotwords]);

  if (settings === null) return null;

  function normalize(raw: string): string[] {
    return raw
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "");
  }

  function commitOnBlur() {
    const next = normalize(draft);
    // Re-render the textarea with the normalized form so the user sees
    // exactly what got saved.
    setDraft(next.join("\n"));
    lastSyncedRef.current = next;
    update({ hotwords: next });
  }

  return (
    <div className="space-y-2 pt-4 border-t border-border">
      <Label htmlFor="hotwords">Vocabulary (hotwords)</Label>
      <p className="text-xs text-muted-foreground">
        Tier 4 (PR #166). One term per line. The list is fed to
        WhisperKit as a prompt so domain words / proper nouns get
        biased toward correct spelling.
      </p>
      <textarea
        id="hotwords"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commitOnBlur}
        placeholder={"Acme Corp\nKubernetes\nplaywright"}
        rows={5}
        className={
          "flex w-full rounded-md border border-border bg-background " +
          "px-3 py-2 text-sm font-mono leading-relaxed " +
          "focus-visible:outline-none focus-visible:ring-2 " +
          "focus-visible:ring-primary"
        }
      />
    </div>
  );
}
