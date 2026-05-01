/**
 * Theme + accent preferences for the Appearance tab.
 *
 * Stored in `localStorage["heron:theme"]` / `localStorage["heron:accent"]`
 * so `index.html`'s `fouc-init.js` can re-apply the choice on next
 * launch before React mounts. The Appearance tab reads via
 * `readThemePref` / `readAccentPref`, writes via `applyTheme` /
 * `applyAccent` (which both update the live DOM and persist).
 */

/**
 * Theme preference stored in localStorage["heron:theme"].
 * "system" (or missing key) → follow prefers-color-scheme.
 * "light"  → force light (no data-theme attribute).
 * "dark"   → force dark (data-theme="dark").
 */
export type ThemePref = "system" | "light" | "dark";

/**
 * Accent color stored in localStorage["heron:accent"].
 * "" (empty / missing key) → bronze (default, baked into @theme).
 */
export type AccentPref = "" | "ink" | "heron" | "sage";

export const THEME_OPTIONS: {
  value: ThemePref;
  label: string;
  description: string;
}[] = [
  {
    value: "system",
    label: "System",
    description: "Follow your macOS appearance setting.",
  },
  {
    value: "light",
    label: "Light",
    description: "Always use the light palette.",
  },
  {
    value: "dark",
    label: "Dark",
    description: "Always use the dark palette.",
  },
];

export const ACCENT_OPTIONS: { value: AccentPref; label: string }[] = [
  { value: "", label: "Bronze (default)" },
  { value: "ink", label: "Ink" },
  { value: "heron", label: "Heron" },
  { value: "sage", label: "Sage" },
];

/**
 * Read the stored theme preference.  Missing key → "system".
 */
export function readThemePref(): ThemePref {
  try {
    const stored = localStorage.getItem("heron:theme");
    if (stored === "light" || stored === "dark" || stored === "system") {
      return stored;
    }
  } catch {
    // localStorage not available (unlikely in Tauri, but be defensive)
  }
  return "system";
}

/**
 * Read the stored accent preference.  Missing key → "" (Bronze).
 */
export function readAccentPref(): AccentPref {
  try {
    const stored = localStorage.getItem("heron:accent");
    if (stored === "ink" || stored === "heron" || stored === "sage") {
      return stored;
    }
  } catch {
    // localStorage not available
  }
  return "";
}

/**
 * Apply a theme pref to the live document and persist it.
 *
 * "system" → resolve via matchMedia immediately so the page reflects
 * the OS preference without a reload; also remove the stored key so
 * fouc-init.js treats the next launch as "system" too.
 *
 * "light" / "dark" → set / clear data-theme and persist.
 */
export function applyTheme(pref: ThemePref): void {
  const html = document.documentElement;

  // Resolve the effective theme to apply to the DOM, independent of storage.
  let effectiveDark: boolean;
  if (pref === "system") {
    effectiveDark =
      window.matchMedia != null &&
      window.matchMedia("(prefers-color-scheme: dark)").matches;
  } else {
    effectiveDark = pref === "dark";
  }

  if (effectiveDark) {
    html.dataset.theme = "dark";
  } else {
    delete html.dataset.theme;
  }

  // Persist the raw preference so fouc-init.js can re-apply it on next launch.
  try {
    if (pref === "system") {
      localStorage.removeItem("heron:theme");
    } else {
      localStorage.setItem("heron:theme", pref);
    }
  } catch {
    // localStorage unavailable — DOM already updated above, so the session
    // reflects the choice even though it won't survive a reload.
  }
}

/**
 * Apply an accent pref to the live document and persist it.
 *
 * "" (Bronze) → remove data-accent and remove the localStorage key
 * so fouc-init.js restores the default on next launch.
 */
export function applyAccent(pref: AccentPref): void {
  const html = document.documentElement;

  // Update the DOM first — independent of storage availability.
  if (pref === "") {
    delete html.dataset.accent;
  } else {
    html.dataset.accent = pref;
  }

  // Persist so fouc-init.js can re-apply on next launch.
  try {
    if (pref === "") {
      localStorage.removeItem("heron:accent");
    } else {
      localStorage.setItem("heron:accent", pref);
    }
  } catch {
    // localStorage unavailable — DOM already updated above.
  }
}
