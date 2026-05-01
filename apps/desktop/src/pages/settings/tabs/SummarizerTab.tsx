import { useState } from "react";

import { Input } from "../../../components/ui/input";
import { Label } from "../../../components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../components/ui/select";
import { Switch } from "../../../components/ui/switch";
import { useSettingsStore } from "../../../store/settings";
import { ANTHROPIC_MODELS, LLM_BACKENDS } from "../constants";
import { KeychainKeyField } from "../sections/KeychainKeyField";

export function SummarizerTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  // The model picker is ephemeral until the Rust `Settings` struct grows
  // an `llm_model` field — local state lets the user move the dropdown
  // visually without losing the change to a hardcoded controlled value.
  // The default is the middle option (Sonnet) which matches the
  // orchestrator's current cost-aware default (phase 41 / #42).
  const [anthropicModel, setAnthropicModel] = useState<string>(
    ANTHROPIC_MODELS[1].value,
  );

  if (settings === null) return null;

  const showAnthropicModelPicker = settings.llm_backend === "anthropic";

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Summarizer</h2>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">LLM backend</legend>
        {/*
          Visual grouping per the UX-redesign IA note: hosted "API
          providers" (billed per-token) and local "CLI" (zero-billed,
          spawn the user's installed binary) are read as different
          billing models. Rendered as two stacked subgroups with a
          subheading each. The data lives on the `LLM_BACKENDS.group`
          discriminator so the order is the array order within each
          group.
        */}
        {(["api", "cli"] as const).map((group) => {
          const heading = group === "api" ? "API providers" : "Local CLI";
          const opts = LLM_BACKENDS.filter((o) => o.group === group);
          return (
            <div key={group} className="space-y-1">
              <div className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
                {heading}
              </div>
              <div className="space-y-2">
                {opts.map((opt) => (
                  <label
                    key={opt.value}
                    className="flex items-center gap-2 text-sm cursor-pointer"
                  >
                    <input
                      type="radio"
                      name="llm-backend"
                      value={opt.value}
                      checked={settings.llm_backend === opt.value}
                      onChange={() => update({ llm_backend: opt.value })}
                      className="h-4 w-4 accent-primary"
                    />
                    {opt.label}
                  </label>
                ))}
              </div>
            </div>
          );
        })}
      </fieldset>

      {showAnthropicModelPicker && (
        <div className="space-y-2">
          <Label htmlFor="anthropic-model">Anthropic model</Label>
          <Select value={anthropicModel} onValueChange={setAnthropicModel}>
            <SelectTrigger id="anthropic-model" className="w-72">
              <SelectValue placeholder="Pick a model" />
            </SelectTrigger>
            <SelectContent>
              {ANTHROPIC_MODELS.map((m) => (
                <SelectItem key={m.value} value={m.value}>
                  {m.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">
            Model selection persists once the orchestrator reads it from
            settings.json (follow-up — the Rust `Settings` struct has no
            `llm_model` field today).
          </p>
        </div>
      )}

      <KeychainKeyField
        account="anthropic_api_key"
        label="Anthropic API key"
        placeholder="sk-ant-…"
        helpText="Stored in the macOS login Keychain. heron never writes API keys to settings.json or any other file on disk."
      />

      <KeychainKeyField
        account="openai_api_key"
        label="OpenAI API key"
        placeholder="sk-…"
        helpText="Used by the OpenAI Realtime backend during meetings and by the Codex CLI summarizer. Stored in the macOS login Keychain — never written to settings.json or any other file on disk."
      />

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="auto-summarize">Auto-summarize on stop</Label>
          <p className="text-xs text-muted-foreground">
            When a recording ends, kick off the summarizer immediately.
            Off means the review UI gets a "summarize" button.
          </p>
        </div>
        <Switch
          id="auto-summarize"
          checked={settings.auto_summarize}
          onCheckedChange={(checked) => update({ auto_summarize: checked })}
        />
      </div>

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="strip-names">Strip participant names</Label>
          <p className="text-xs text-muted-foreground">
            Tier 4 (PR #167). Replace `display_name` with `Speaker A/B/C`
            in the transcript before sending it to the LLM. Privacy
            mitigation; the on-disk transcript is unchanged.
          </p>
        </div>
        <Switch
          id="strip-names"
          checked={settings.strip_names_before_summarization}
          onCheckedChange={(checked) =>
            update({ strip_names_before_summarization: checked })
          }
        />
      </div>

      <div className="space-y-2">
        <Label htmlFor="openai-model">OpenAI model</Label>
        <Input
          id="openai-model"
          value={settings.openai_model}
          onChange={(e) => update({ openai_model: e.target.value })}
          placeholder="gpt-4o-mini"
          className="font-mono"
        />
        <p className="text-xs text-muted-foreground">
          Used when the LLM backend is OpenAI. The OpenAI summarizer
          (PR #156) sends this as the `model` field on
          `/v1/chat/completions`.
        </p>
      </div>
    </section>
  );
}
