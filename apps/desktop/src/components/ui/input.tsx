/**
 * shadcn/ui-flavored Input.
 *
 * Plain `<input>` with the same focus-ring + disabled-state idiom the
 * Button component uses, so a Label/Input/Button row reads as one
 * design-system family rather than three different aesthetics.
 *
 * React 19 forwards `ref` as a regular prop — no `forwardRef` shim.
 */

import type * as React from "react";

import { cn } from "../../lib/cn";

export interface InputProps
  extends React.InputHTMLAttributes<HTMLInputElement> {
  ref?: React.Ref<HTMLInputElement>;
}

export function Input({ className, type, ref, ...props }: InputProps) {
  return (
    <input
      ref={ref}
      type={type}
      className={cn(
        "flex h-9 w-full rounded-md border border-border bg-background px-3 py-1 text-sm",
        "placeholder:text-muted-foreground",
        "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
        "disabled:cursor-not-allowed disabled:opacity-50",
        className,
      )}
      {...props}
    />
  );
}
