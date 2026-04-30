/**
 * Persistent "Ask" bar for the Home page (Granola-pattern, local-first
 * phrasing). Two layers:
 *
 *   1. A row of one-tap "recipe" chips for common cross-vault prompts
 *      ("List recent todos", "Draft follow-up", …) — clicking a chip
 *      fills the input.
 *   2. A serif italic prompt input with a model-picker label, a
 *      voice-prompt button, and an Ask submit.
 *
 * The cross-vault Ask backend (RAG over markdown summaries, prompt
 * routing) isn't wired yet — the gap analysis flags this as a
 * net-new system gated on architectural decisions. Per the redesign
 * plan we ship the chrome with explicit "not wired yet" feedback so
 * the new IA is visible without lying about capability. Submitting
 * the form, hitting Voice, or accepting the suggestions all toast
 * the same friendly stub message.
 */

import { useEffect, useRef, useState } from "react";
import {
  BookOpen,
  CalendarDays,
  CheckCircle,
  Eye,
  Mail,
  Mic,
  Plus,
  Send,
  Sparkles,
  Users,
  type LucideIcon,
} from "lucide-react";
import { toast } from "sonner";

interface Recipe {
  id: string;
  label: string;
  icon: LucideIcon;
  prompt: string;
}

const RECIPES: Recipe[] = [
  {
    id: "todos",
    label: "List recent todos",
    icon: CheckCircle,
    prompt: "Pull every action item assigned to me from the last two weeks.",
  },
  {
    id: "followup",
    label: "Draft follow-up",
    icon: Mail,
    prompt:
      "Draft a follow-up email summarizing decisions and next steps from my most recent meeting.",
  },
  {
    id: "recap",
    label: "Weekly recap",
    icon: BookOpen,
    prompt: "Write a 5-bullet recap of this week's meetings.",
  },
  {
    id: "prep",
    label: "Prep for next meeting",
    icon: CalendarDays,
    prompt: "Read upcoming calendar and prep notes for my next meeting.",
  },
  {
    id: "coach",
    label: "Coach me",
    icon: Users,
    prompt:
      "Surface unresolved threads from my 1:1s across the last 6 weeks.",
  },
  {
    id: "blind",
    label: "Blind spots",
    icon: Eye,
    prompt:
      "What am I missing? Look across last week's notes for unfinished threads.",
  },
];

const VISIBLE_CHIPS = 4;
const STUB_MESSAGE =
  "Ask isn't wired yet — cross-vault Q&A is coming with vault search.";

export function AskBar() {
  const [value, setValue] = useState("");
  const [focused, setFocused] = useState(false);
  const [drawerOpen, setDrawerOpen] = useState(false);
  // Two refs: the drawer panel itself (so clicks INSIDE it stay) and
  // the toggle button (so the toggle's own onClick can flip the
  // state without our outside-click handler also firing). Refs on
  // the toggle button are needed because a `click` listener at the
  // document level fires AFTER the toggle's React onClick during the
  // same tick — without excluding the toggle, both would call
  // `setDrawerOpen(false)` and the user could never reopen the
  // drawer in a single tap.
  const drawerRef = useRef<HTMLDivElement | null>(null);
  const toggleRef = useRef<HTMLButtonElement | null>(null);

  // Close the recipes drawer when the user clicks anywhere that
  // ISN'T the drawer or the toggle, or when they press Escape.
  // Clicking the ask input, a visible recipe chip, the model
  // picker, the mic button, or the Ask submit all dismiss the
  // drawer because none of those are inside `drawerRef`. This
  // matches the muscle memory users have for popovers in similar
  // tools. Escape additionally returns focus to the toggle so a
  // keyboard-only user is never stranded.
  //
  // Uses `click` (rather than `mousedown`) because React's onClick
  // for the toggle fires during the same tick, and React 17+
  // delegates events at the root container — `click` lets the
  // toggle's own setDrawerOpen call run before we read the target
  // here, and the listener is only attached while the drawer is
  // open so a closed-drawer click is free.
  useEffect(() => {
    if (!drawerOpen) return;
    const onClick = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (target === null) return;
      if (drawerRef.current?.contains(target)) return;
      if (toggleRef.current?.contains(target)) return;
      setDrawerOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // Only return focus to the toggle if the user was actually
      // inside the drawer when they pressed Escape — yanking focus
      // away from the ask input mid-typing would be hostile.
      const focusInsideDrawer =
        drawerRef.current?.contains(document.activeElement) ?? false;
      setDrawerOpen(false);
      if (focusInsideDrawer) toggleRef.current?.focus();
    };
    document.addEventListener("click", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("click", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [drawerOpen]);

  function applyRecipe(recipe: Recipe) {
    setValue(recipe.prompt);
    setDrawerOpen(false);
  }

  function onSubmit(event: React.FormEvent) {
    event.preventDefault();
    if (!value.trim()) return;
    toast.info(STUB_MESSAGE);
  }

  return (
    <section
      className="relative shrink-0 border-t px-14 py-3"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-rule)",
      }}
    >
      <div className="mb-2.5 flex flex-wrap items-center gap-2">
        <span
          className="mr-1 font-mono text-[10.5px] uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          Recipes
        </span>
        {RECIPES.slice(0, VISIBLE_CHIPS).map((recipe) => (
          <RecipeChip
            key={recipe.id}
            recipe={recipe}
            onClick={() => applyRecipe(recipe)}
          />
        ))}
        <button
          ref={toggleRef}
          type="button"
          onClick={() => setDrawerOpen((v) => !v)}
          aria-expanded={drawerOpen}
          aria-haspopup="menu"
          className="inline-flex items-center gap-1.5 rounded-full border-dashed border px-2.5 py-1 text-[11.5px] transition-colors hover:bg-paper-3"
          style={{
            color: "var(--color-ink-3)",
            borderColor: "var(--color-rule-2)",
            background: "var(--color-paper)",
          }}
        >
          <Plus size={11} aria-hidden="true" />
          all {RECIPES.length}
        </button>
        <span className="flex-1" />
        <span
          className="font-mono text-[10.5px]"
          style={{ color: "var(--color-ink-3)" }}
        >
          runs locally where possible · LLM only on summarize
        </span>
      </div>

      {drawerOpen && (
        // Plain labelled popover rather than `role="menu"` —
        // implementing the full ARIA menu pattern would mean
        // arrow-key navigation, typeahead, focus traps, the works,
        // and the chrome here is just a list of buttons. Without
        // that machinery `role="menu"` reads worse to screen
        // readers than the default semantics.
        <div
          ref={drawerRef}
          aria-label="All recipes"
          className="absolute z-50 grid grid-cols-2 gap-1.5 rounded-md border p-2.5 shadow-lg"
          style={{
            // Anchor above the bar so the drawer never extends below
            // the viewport.
            bottom: "calc(100% + 6px)",
            left: 56,
            minWidth: 460,
            background: "var(--color-paper)",
            borderColor: "var(--color-rule-2)",
          }}
        >
          {RECIPES.map((recipe) => {
            const Icon = recipe.icon;
            return (
              <button
                type="button"
                key={recipe.id}
                onClick={() => applyRecipe(recipe)}
                className="flex items-start gap-2 rounded p-2 text-left transition-colors hover:bg-paper-2"
              >
                <Icon
                  size={12}
                  aria-hidden="true"
                  className="mt-0.5"
                  style={{ color: "var(--color-accent)" }}
                />
                <div className="min-w-0">
                  <div
                    className="text-[12.5px] font-medium"
                    style={{ color: "var(--color-ink)" }}
                  >
                    {recipe.label}
                  </div>
                  <div
                    className="mt-0.5 font-mono text-[10.5px] leading-snug"
                    style={{ color: "var(--color-ink-3)" }}
                  >
                    {recipe.prompt}
                  </div>
                </div>
              </button>
            );
          })}
        </div>
      )}

      <form
        onSubmit={onSubmit}
        className="flex items-center gap-2 rounded-md border py-2 pl-3.5 pr-2 transition-colors"
        style={{
          background: "var(--color-paper)",
          borderColor: focused
            ? "var(--color-accent)"
            : "var(--color-rule-2)",
        }}
      >
        <Sparkles
          size={14}
          aria-hidden="true"
          style={{ color: "var(--color-accent)" }}
        />
        <input
          type="text"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onFocus={() => setFocused(true)}
          onBlur={() => setFocused(false)}
          aria-label="Ask anything across your vault"
          placeholder="Ask anything across your vault — “what did Iris flag last week?”"
          className="min-w-0 flex-1 border-0 bg-transparent px-1.5 py-1 outline-none"
          style={{
            color: "var(--color-ink)",
            fontFamily: "var(--font-serif)",
            fontSize: 14,
            fontStyle: "italic",
          }}
        />
        <span
          className="px-1.5 font-mono text-[10px]"
          style={{ color: "var(--color-ink-4)" }}
          title="Model — picker not wired yet"
        >
          claude-haiku-4-5 ▾
        </span>
        <button
          type="button"
          aria-label="Voice prompt"
          onClick={() => toast.info(STUB_MESSAGE)}
          className="inline-flex h-7 w-7 items-center justify-center rounded transition-colors hover:bg-paper-2"
          style={{ color: "var(--color-ink-3)" }}
        >
          <Mic size={13} aria-hidden="true" />
        </button>
        <button
          type="submit"
          disabled={!value.trim()}
          className="inline-flex items-center gap-1.5 rounded border px-3 py-1 text-[12px] transition-colors disabled:opacity-50"
          style={{
            background: "var(--color-paper)",
            borderColor: "var(--color-rule-2)",
            color: "var(--color-ink)",
          }}
        >
          <Send size={11} aria-hidden="true" />
          Ask
        </button>
      </form>
    </section>
  );
}

function RecipeChip({
  recipe,
  onClick,
}: {
  recipe: Recipe;
  onClick: () => void;
}) {
  const Icon = recipe.icon;
  return (
    <button
      type="button"
      onClick={onClick}
      className="inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-[11.5px] transition-colors hover:bg-paper-3"
      style={{
        background: "var(--color-paper)",
        borderColor: "var(--color-rule-2)",
        color: "var(--color-ink-2)",
      }}
    >
      <Icon
        size={11}
        aria-hidden="true"
        style={{ color: "var(--color-accent)" }}
      />
      <span>{recipe.label}</span>
    </button>
  );
}
