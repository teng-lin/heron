/**
 * Resolve where an onboarded user should land.
 *
 * Used by both `App.tsx`'s `<FirstRunGate>` (cold-start landing) and
 * `pages/Onboarding.tsx`'s `Finish setup` button (post-wizard
 * landing). Centralized so the policy ("latest note > /home") stays
 * consistent and a future addition (e.g. a recording-in-progress
 * shortcut) lands in one place rather than two.
 *
 * The probe is best-effort: any IPC / serialization failure falls
 * through to `/home`. Failing closed (re-onboarding) on a transient
 * vault read would be worse than landing on the placeholder.
 *
 * The returned path is already URL-safe: session IDs from
 * `heron_last_note_session_id` are basename-validated server-side
 * (PR-η phase 69), but `encodeURIComponent` is belt-and-suspenders
 * against a future schema where IDs gain `:` or `/` characters.
 */

import { invoke } from "./invoke";

/** Default landing for an onboarded user with no notes yet. */
const HOME_PATH = "/home";

/**
 * Resolve the post-onboarding destination route. Always returns a
 * concrete path string — never throws. IPC failures degrade to
 * `/home` rather than propagating.
 */
export async function resolvePostOnboardingDestination(): Promise<string> {
  try {
    const lastId = await invoke("heron_last_note_session_id");
    if (lastId !== null && lastId.length > 0) {
      return `/review/${encodeURIComponent(lastId)}`;
    }
  } catch {
    // Vault probe failure → fall through to /home. Logged at the
    // call site if needed; keeping the helper silent keeps the
    // contract "best-effort, never throws".
  }
  return HOME_PATH;
}
