/**
 * Class-name helper used across the React tree.
 *
 * Standard shadcn/ui pattern: `clsx` resolves conditionals, then
 * `tailwind-merge` deduplicates conflicting Tailwind utilities (e.g.
 * `px-2 px-4` collapses to `px-4`). Components that compose Tailwind
 * classes with caller overrides should funnel both through `cn()` so
 * the override wins regardless of declaration order.
 */

import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
