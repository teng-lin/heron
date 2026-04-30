/**
 * "Spaces" tab strip on Home.
 *
 * Spaces are the redesign's IA for vault sharding — a private
 * `personal` space plus N shared collaborator-scoped notebooks
 * (Investors / Customers / Team). The backend for shared spaces does
 * not exist yet (no collaborator identity, no per-space file
 * mappings, no sharing wire), so we render the strip with one real
 * personal space + a few honestly-labelled `soon` tabs so the new IA
 * is visible without lying about capability. Per the redesign Q4
 * decision: stub-with-mocked-data, label honestly.
 *
 * The personal-space count is read from `useMeetingsStore` — every
 * meeting today belongs to "My notes" because the vault is a single
 * root (`Settings.vault_root`).
 */

import { Lock, Plus } from "lucide-react";
import { toast } from "sonner";

import { useMeetingsStore } from "../../store/meetings";

interface ComingSpace {
  id: string;
  label: string;
  swatch: string;
}

const COMING_SPACES: ComingSpace[] = [
  { id: "investors", label: "Investor calls", swatch: "#8b6f3e" },
  { id: "customers", label: "Customer calls", swatch: "#3d6a78" },
  { id: "team", label: "Team standups", swatch: "#5a7a52" },
];

const SHARED_SOON =
  "Shared spaces are coming once collaborator identity lands.";

export function SpacesStrip() {
  const meetingsCount = useMeetingsStore((s) => s.items.length);
  return (
    <nav
      aria-label="Spaces"
      className="flex items-center border-b px-14"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule)",
      }}
    >
      <PersonalTab count={meetingsCount} />
      {COMING_SPACES.map((space) => (
        <ComingTab key={space.id} space={space} />
      ))}
      <span className="flex-1" />
      <button
        type="button"
        onClick={() => toast.info(SHARED_SOON)}
        className="inline-flex items-center gap-1.5 rounded px-2 py-1 text-[11.5px] transition-colors hover:bg-paper-2"
        style={{ color: "var(--color-ink-3)" }}
        title={SHARED_SOON}
      >
        <Plus size={12} aria-hidden="true" />
        New space
      </button>
    </nav>
  );
}

/**
 * The "real" tab — selected by default and the only one that actually
 * filters anything (today the vault is a single root, so this is a
 * no-op until shared spaces ship). Bottom-border accent matches the
 * prototype's segmented-tab geometry.
 */
function PersonalTab({ count }: { count: number }) {
  return (
    <button
      type="button"
      aria-current="page"
      className="-mb-px inline-flex items-center gap-2 px-4 py-3.5 transition-colors"
      style={{
        background: "transparent",
        borderBottom: "2px solid var(--color-accent)",
        color: "var(--color-ink)",
      }}
    >
      <Lock
        size={12}
        aria-hidden="true"
        style={{ color: "var(--color-accent)" }}
      />
      <span
        className="font-serif text-[14px] font-medium"
        style={{ letterSpacing: "-0.005em" }}
      >
        My notes
      </span>
      <span
        className="font-mono text-[10px]"
        style={{ color: "var(--color-ink-4)" }}
      >
        {count}
      </span>
    </button>
  );
}

/** Ghosted tab — visible to convey the IA, but `disabled` and toasts. */
function ComingTab({ space }: { space: ComingSpace }) {
  return (
    <button
      type="button"
      onClick={() => toast.info(SHARED_SOON)}
      title={SHARED_SOON}
      className="-mb-px inline-flex items-center gap-2 px-4 py-3.5 transition-colors hover:bg-paper-2"
      style={{
        background: "transparent",
        borderBottom: "2px solid transparent",
        color: "var(--color-ink-4)",
      }}
    >
      <span
        aria-hidden="true"
        className="inline-block"
        style={{
          width: 9,
          height: 9,
          borderRadius: 2,
          background: space.swatch,
          opacity: 0.55,
        }}
      />
      <span
        className="font-serif text-[14px] font-normal"
        style={{ letterSpacing: "-0.005em" }}
      >
        {space.label}
      </span>
      <span
        className="font-mono text-[9px] uppercase tracking-[0.08em]"
        style={{ color: "var(--color-ink-4)" }}
      >
        soon
      </span>
    </button>
  );
}
