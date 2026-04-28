import { HeronMark } from "./heron-mark";

export interface HeronWordmarkProps {
  size?: number;
  className?: string;
}

export function HeronWordmark({ size = 16, className }: HeronWordmarkProps) {
  return (
    <span
      className={className}
      style={{ display: "inline-flex", alignItems: "center", gap: 7 }}
    >
      <HeronMark size={size + 2} />
      <span
        style={{
          fontFamily: "var(--font-serif)",
          fontSize: size,
          fontWeight: 500,
          letterSpacing: "0.01em",
          color: "var(--color-ink)",
        }}
      >
        heron
      </span>
    </span>
  );
}
