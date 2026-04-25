/**
 * shadcn/ui-flavored Button.
 *
 * PR-α (phase 62) ships exactly one button component as a starter for
 * the design-system pattern the rest of the React tree will follow:
 *
 * - `Slot` lets callers swap the rendered element via `asChild`, which
 *   is how we get a `<Link>` styled as a button without polluting the
 *   button's API surface.
 * - `cn()` merges Tailwind classes so caller-supplied `className`
 *   overrides our defaults.
 * - The `variants`/`sizes` maps below are hand-written rather than
 *   driven by `class-variance-authority` — `cva` is overkill for a
 *   single component. PR-δ promotes the pattern to `cva` if the
 *   variant matrix grows.
 * - React 19 lets function components accept `ref` as a regular prop,
 *   so this component skips `forwardRef` entirely (deprecated in 19,
 *   slated for removal in a future release).
 */

import type * as React from "react";
import { Slot } from "@radix-ui/react-slot";

import { cn } from "../../lib/cn";

type ButtonVariant = "default" | "destructive" | "outline" | "ghost";
type ButtonSize = "default" | "sm" | "lg" | "icon";

const variants: Record<ButtonVariant, string> = {
  default: "bg-primary text-primary-foreground hover:opacity-90",
  destructive:
    "bg-destructive text-destructive-foreground hover:opacity-90",
  outline:
    "border border-border bg-background hover:bg-muted hover:text-foreground",
  ghost: "hover:bg-muted hover:text-foreground",
};

const sizes: Record<ButtonSize, string> = {
  default: "h-9 px-4 py-2",
  sm: "h-8 rounded-md px-3 text-xs",
  lg: "h-10 rounded-md px-8",
  icon: "h-9 w-9",
};

const baseClasses =
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md " +
  "text-sm font-medium transition-colors focus-visible:outline-none " +
  "focus-visible:ring-2 focus-visible:ring-primary disabled:pointer-events-none " +
  "disabled:opacity-50";

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  /**
   * Render the button as its child element (preserving the styling).
   * Useful for wrapping `<Link>` or `<a>` without nesting.
   */
  asChild?: boolean;
  ref?: React.Ref<HTMLButtonElement>;
}

export function Button({
  className,
  variant = "default",
  size = "default",
  asChild = false,
  type,
  ref,
  ...props
}: ButtonProps) {
  const Comp = asChild ? Slot : "button";
  // Bare `<button>` defaults to `type="submit"` inside a form, which
  // would silently submit any future Settings form on click. Default
  // to `"button"` when rendering an actual button element; let `Slot`
  // pass through whatever the child element wants.
  const resolvedType = asChild ? type : (type ?? "button");
  return (
    <Comp
      ref={ref}
      type={resolvedType}
      className={cn(baseClasses, variants[variant], sizes[size], className)}
      {...props}
    />
  );
}
