/**
 * shadcn/ui-flavored Label.
 *
 * Thin wrapper around the native `<label>` element. We don't pull in
 * `@radix-ui/react-label` for v1 — Radix's Label adds click-forwarding
 * to a non-input descendant, which we don't need: every Settings field
 * pairs a label with a focusable form control via `htmlFor`/`id`, so
 * the browser's built-in label↔control association is sufficient.
 *
 * Sized to match `<input>`'s `text-sm` body type so a label-then-input
 * column stays vertically rhythmic without per-field overrides.
 */

import type * as React from "react";

import { cn } from "../../lib/cn";

export type LabelProps = React.LabelHTMLAttributes<HTMLLabelElement>;

export function Label({ className, ...props }: LabelProps) {
  return (
    <label
      className={cn(
        "text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70",
        className,
      )}
      {...props}
    />
  );
}
