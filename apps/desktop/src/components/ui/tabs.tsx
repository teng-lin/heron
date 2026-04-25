/**
 * shadcn/ui-flavored Tabs.
 *
 * Wraps `@radix-ui/react-tabs` with project Tailwind classes. Default
 * styling is horizontal; the Settings pane's vertical left-rail uses
 * the `orientation="vertical"` Radix prop on `<Tabs>` which the list +
 * trigger styling here adapts to via `data-[orientation=vertical]`
 * variants.
 *
 * Re-exports Radix's primitives so callers stay close to the upstream
 * API surface (no extra abstraction layer to remember).
 */

import type * as React from "react";
import * as TabsPrimitive from "@radix-ui/react-tabs";

import { cn } from "../../lib/cn";

export const Tabs = TabsPrimitive.Root;

export interface TabsListProps
  extends React.ComponentPropsWithoutRef<typeof TabsPrimitive.List> {
  ref?: React.Ref<React.ElementRef<typeof TabsPrimitive.List>>;
}

export function TabsList({ className, ref, ...props }: TabsListProps) {
  return (
    <TabsPrimitive.List
      ref={ref}
      className={cn(
        "inline-flex items-center justify-start rounded-md bg-muted p-1 text-muted-foreground",
        "data-[orientation=vertical]:flex-col data-[orientation=vertical]:items-stretch",
        "data-[orientation=horizontal]:h-9",
        className,
      )}
      {...props}
    />
  );
}

export interface TabsTriggerProps
  extends React.ComponentPropsWithoutRef<typeof TabsPrimitive.Trigger> {
  ref?: React.Ref<React.ElementRef<typeof TabsPrimitive.Trigger>>;
}

export function TabsTrigger({ className, ref, ...props }: TabsTriggerProps) {
  return (
    <TabsPrimitive.Trigger
      ref={ref}
      className={cn(
        "inline-flex items-center justify-start gap-2 whitespace-nowrap rounded-sm px-3 py-1.5",
        "text-sm font-medium transition-all",
        "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
        "disabled:pointer-events-none disabled:opacity-50",
        "data-[state=active]:bg-background data-[state=active]:text-foreground data-[state=active]:shadow-sm",
        "data-[orientation=vertical]:justify-start",
        className,
      )}
      {...props}
    />
  );
}

export interface TabsContentProps
  extends React.ComponentPropsWithoutRef<typeof TabsPrimitive.Content> {
  ref?: React.Ref<React.ElementRef<typeof TabsPrimitive.Content>>;
}

export function TabsContent({ className, ref, ...props }: TabsContentProps) {
  return (
    <TabsPrimitive.Content
      ref={ref}
      className={cn(
        "mt-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
        className,
      )}
      {...props}
    />
  );
}
