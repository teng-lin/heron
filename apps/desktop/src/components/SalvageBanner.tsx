/**
 * Crash-recovery banner — mounted at the very top of the app shell
 * (see `App.tsx`) above the route switch.
 *
 * Phase 75 (PR-ν). Replaces the previous Sonner toast that fired on
 * mount when `useSalvagePrompt` found unfinalized sessions: a toast
 * auto-dismisses after ~10 s and is easy to miss when the user is
 * mid-task. A sticky banner that persists until the user either opens
 * `/salvage` or explicitly dismisses it is a louder, harder-to-miss
 * surface for "you have data we can still recover".
 *
 * Visibility rules
 * ----------------
 *
 * The banner renders iff **all four** are true:
 *
 *   1. The scan has run (`promptedThisSession === true`). Without this
 *      we'd render a stale "0 sessions" pre-scan flash on every cold
 *      start.
 *   2. There is at least one unfinalized session (`unfinalizedCount > 0`).
 *      The Salvage page can drive this back to zero after a per-row
 *      purge, in which case the banner disappears without the user
 *      having to dismiss it.
 *   3. The user has not clicked "Dismiss" this launch. The dismiss
 *      flag is per-launch only (see `store/salvage.ts`); a future
 *      launch with the same unfinalized sessions re-shows the banner.
 *   4. The user is not currently on `/salvage`. Showing "N sessions
 *      need recovery" with an "Open Salvage" button while the user is
 *      already looking at that exact list is redundant.
 *
 * The dismiss button is intentionally a regular click (not a long-
 * press / not a confirm dialog): the user is opting out of the *banner*,
 * not out of the recovery itself — the data is still there and a tray
 * "Open last note…" or a manual `/salvage` visit recovers it.
 */

import { AlertTriangle } from "lucide-react";
import { useLocation, useNavigate } from "react-router-dom";

import { Button } from "./ui/button";
import { useSalvagePromptStore } from "../store/salvage";

export default function SalvageBanner() {
  const promptedThisSession = useSalvagePromptStore(
    (s) => s.promptedThisSession,
  );
  const unfinalizedCount = useSalvagePromptStore((s) => s.unfinalizedCount);
  const dismissed = useSalvagePromptStore((s) => s.dismissed);
  const dismiss = useSalvagePromptStore((s) => s.dismiss);
  const navigate = useNavigate();
  const location = useLocation();

  // Suppress on `/salvage` itself — the banner's "Open Salvage" button
  // would just navigate to the same page, and the user is already
  // looking at the full list. The store-level state stays intact so a
  // navigate-away-then-back keeps the banner visible until the user
  // either dismisses or empties the list.
  if (
    !promptedThisSession ||
    unfinalizedCount <= 0 ||
    dismissed ||
    location.pathname === "/salvage"
  ) {
    return null;
  }

  // Pluralise without dragging in a dependency. The count is always a
  // positive integer at this point (negative / non-integer values would
  // have failed the visibility check above), so the simple `=== 1` test
  // is sufficient. Verb plurals match the noun: "1 session needs" vs
  // "2 sessions need".
  const sessionsLabel = unfinalizedCount === 1 ? "session" : "sessions";
  const verb = unfinalizedCount === 1 ? "needs" : "need";

  return (
    <div
      role="alert"
      // Yellow tokens are the closest match in the existing Tailwind v4
      // theme to "warning, not error" — `destructive` would over-state
      // a recoverable situation. The border/text contrast lands on the
      // yellow-500/30 + yellow-800 pair shadcn ships for warning toasts
      // so the banner blends with the rest of the design language.
      className={
        "flex items-center justify-between gap-4 border-b border-yellow-500/30 " +
        "bg-yellow-500/10 px-4 py-2 text-sm text-yellow-800"
      }
    >
      <div className="flex min-w-0 items-center gap-2">
        <AlertTriangle
          className="h-4 w-4 shrink-0"
          aria-hidden="true"
        />
        <span className="truncate">
          {unfinalizedCount} {sessionsLabel} {verb} recovery
        </span>
      </div>
      <div className="flex shrink-0 gap-2">
        <Button
          size="sm"
          variant="outline"
          onClick={() => navigate("/salvage")}
        >
          Open Salvage
        </Button>
        <Button size="sm" variant="ghost" onClick={dismiss}>
          Dismiss
        </Button>
      </div>
    </div>
  );
}
