/**
 * shadcn/ui-flavored Switch.
 *
 * Wraps `@radix-ui/react-switch` with the project's Tailwind palette.
 * Used by toggleable preferences (auto-summarize, recover-on-launch,
 * crash-telemetry, the §14.2 disclosure-banner reminder, etc.).
 *
 * Uses controlled `checked` + `onCheckedChange` per Radix's
 * convention, so the parent owns the boolean and a Settings-store
 * `update({ field: value })` call is the only state-mutation path.
 */

import type * as React from "react";
import * as SwitchPrimitive from "@radix-ui/react-switch";

import { cn } from "../../lib/cn";

export interface SwitchProps
  extends React.ComponentPropsWithoutRef<typeof SwitchPrimitive.Root> {
  ref?: React.Ref<React.ElementRef<typeof SwitchPrimitive.Root>>;
}

export function Switch({ className, ref, ...props }: SwitchProps) {
  return (
    <SwitchPrimitive.Root
      ref={ref}
      className={cn(
        "peer inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full",
        "border-2 border-transparent transition-colors",
        "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
        "disabled:cursor-not-allowed disabled:opacity-50",
        "data-[state=checked]:bg-primary data-[state=unchecked]:bg-muted-foreground/40",
        className,
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb
        className={cn(
          "pointer-events-none block h-4 w-4 rounded-full bg-background shadow-sm",
          "ring-0 transition-transform",
          "data-[state=checked]:translate-x-4 data-[state=unchecked]:translate-x-0",
        )}
      />
    </SwitchPrimitive.Root>
  );
}
