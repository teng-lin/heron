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

  if (settings === null) return null;

  // Render as newline-joined text in the textarea. Splitting on save —
  // `\n`-delimited string mid-edit would jitter the cursor as the user
  // typed; the controlled value here is pure local string state derived
  // from the Vec<String>.
  const value = settings.hotwords.join("\n");

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
        value={value}
        onChange={(e) => {
          const next = e.target.value
            .split("\n")
            .map((line) => line.trim())
            .filter((line) => line !== "");
          update({ hotwords: next });
        }}
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
