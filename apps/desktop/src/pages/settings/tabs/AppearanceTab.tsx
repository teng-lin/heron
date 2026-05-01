import { useEffect, useState } from "react";

import {
  ACCENT_OPTIONS,
  THEME_OPTIONS,
  applyAccent,
  applyTheme,
  readAccentPref,
  readThemePref,
  type AccentPref,
  type ThemePref,
} from "../utils/theme";

export function AppearanceTab() {
  const [theme, setTheme] = useState<ThemePref>(readThemePref);
  const [accent, setAccent] = useState<AccentPref>(readAccentPref);

  // Keep the DOM in sync with OS theme changes while "system" is selected.
  useEffect(() => {
    if (theme !== "system") return;
    const mq = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!mq) return;
    const handler = () => applyTheme("system");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, [theme]);

  function handleThemeChange(value: ThemePref) {
    setTheme(value);
    applyTheme(value);
  }

  function handleAccentChange(value: AccentPref) {
    setAccent(value);
    applyAccent(value);
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Appearance</h2>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">Theme</legend>
        <div className="space-y-2">
          {THEME_OPTIONS.map((opt) => (
            <label
              key={opt.value}
              className="flex items-start gap-2 text-sm cursor-pointer"
            >
              <input
                type="radio"
                name="theme"
                value={opt.value}
                checked={theme === opt.value}
                onChange={() => handleThemeChange(opt.value)}
                className="mt-0.5 h-4 w-4 accent-primary"
              />
              <div>
                <div>{opt.label}</div>
                <div className="text-xs text-muted-foreground">
                  {opt.description}
                </div>
              </div>
            </label>
          ))}
        </div>
      </fieldset>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">Accent color</legend>
        <div className="space-y-2">
          {ACCENT_OPTIONS.map((opt) => (
            <label
              key={opt.value === "" ? "bronze" : opt.value}
              className="flex items-center gap-2 text-sm cursor-pointer"
            >
              <input
                type="radio"
                name="accent"
                value={opt.value}
                checked={accent === opt.value}
                onChange={() => handleAccentChange(opt.value)}
                className="h-4 w-4 accent-primary"
              />
              {opt.label}
            </label>
          ))}
        </div>
        <p className="text-xs text-muted-foreground">
          Changes apply immediately. Reload is not required.
        </p>
      </fieldset>
    </section>
  );
}
