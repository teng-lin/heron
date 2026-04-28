/**
 * Deterministic avatar color + initials. Matches the prototype's
 * `.design/atoms.jsx` palette so visual identity is stable across the
 * port. The hash is intentionally tiny — names are short, collisions
 * inside a single meeting's participant list are rare and harmless.
 */

const AVATAR_PALETTE = [
  "#8b6f3e",
  "#3d6a78",
  "#5a7a52",
  "#a85e4f",
  "#6b5e8e",
  "#7a6e5e",
  "#4a6a64",
  "#8e6b5a",
] as const;

export function avatarColor(name: string): string {
  let h = 0;
  for (let i = 0; i < name.length; i++) {
    h = (h * 31 + name.charCodeAt(i)) >>> 0;
  }
  return AVATAR_PALETTE[h % AVATAR_PALETTE.length];
}

export function initials(name: string): string {
  return name
    .split(/\s+/)
    .slice(0, 2)
    .map((w) => w[0] ?? "")
    .join("")
    .toUpperCase();
}
