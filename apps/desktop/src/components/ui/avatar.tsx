import { avatarColor, initials } from "../../lib/avatar";
import { cn } from "../../lib/cn";

export interface AvatarProps {
  name: string;
  size?: number;
  className?: string;
}

export function Avatar({ name, size = 22, className }: AvatarProps) {
  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-full font-mono font-semibold text-[var(--color-paper)]",
        className,
      )}
      style={{
        width: size,
        height: size,
        fontSize: size * 0.42,
        background: avatarColor(name),
      }}
      aria-label={name}
    >
      {initials(name)}
    </span>
  );
}
