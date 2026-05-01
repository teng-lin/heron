/**
 * Map a `KeyboardEvent.key` value to Tauri's
 * `tauri-plugin-global-shortcut` key spelling. The browser's `key` is
 * mostly correct already (`F1`, `R`, `Enter`); a small alias table
 * covers the spaces ("ArrowLeft" → "Left") that diverge.
 */
export function normalizeKey(key: string): string {
  // Space is `length === 1` but Tauri's parser wants the literal
  // word "Space", so it has to be handled before the single-char
  // uppercase fast path.
  if (key === " ") {
    return "Space";
  }
  if (key.length === 1) {
    return key.toUpperCase();
  }
  switch (key) {
    case "ArrowLeft":
      return "Left";
    case "ArrowRight":
      return "Right";
    case "ArrowUp":
      return "Up";
    case "ArrowDown":
      return "Down";
    default:
      return key;
  }
}
