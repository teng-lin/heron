import { useRef } from "react";
import { useNavigate } from "react-router-dom";

import type { ActiveMode } from "../../lib/invoke";
import { cn } from "../../lib/cn";
import { setActiveMode, useActiveMode } from "../../store/mode";

interface ModeOption {
  id: ActiveMode;
  label: string;
  status: "alpha" | "coming";
  /** Where the pill navigates when clicked. Clio returns to the meetings library. */
  route: string;
}

const MODES: ModeOption[] = [
  { id: "clio", label: "Clio", status: "alpha", route: "/home" },
  { id: "athena", label: "Athena", status: "coming", route: "/athena" },
  { id: "pollux", label: "Pollux", status: "coming", route: "/pollux" },
];

export function ModePill() {
  const active = useActiveMode();
  const navigate = useNavigate();
  const refs = useRef<(HTMLButtonElement | null)[]>([]);

  const select = (option: ModeOption) => {
    void setActiveMode(option.id);
    navigate(option.route);
  };

  // Roving-tabindex arrow-key navigation, per WAI-ARIA radiogroup
  // pattern: ArrowLeft/Right and Home/End move focus AND activate the
  // newly-focused radio (matching native <input type="radio"> behavior
  // in a <fieldset>). Tab moves out of the group entirely.
  const onKeyDown = (
    e: React.KeyboardEvent<HTMLButtonElement>,
    idx: number,
  ) => {
    let next = idx;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      next = (idx + 1) % MODES.length;
    } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
      next = (idx - 1 + MODES.length) % MODES.length;
    } else if (e.key === "Home") {
      next = 0;
    } else if (e.key === "End") {
      next = MODES.length - 1;
    } else {
      return;
    }
    e.preventDefault();
    refs.current[next]?.focus();
    select(MODES[next]);
  };

  return (
    <div
      role="radiogroup"
      aria-label="Companion mode"
      className="inline-flex rounded-md border p-0.5"
      style={{
        background: "var(--color-paper-3)",
        borderColor: "var(--color-rule)",
      }}
    >
      {MODES.map((option, idx) => {
        const selected = option.id === active;
        return (
          <button
            key={option.id}
            ref={(el) => {
              refs.current[idx] = el;
            }}
            type="button"
            role="radio"
            aria-checked={selected}
            tabIndex={selected ? 0 : -1}
            onClick={() => select(option)}
            onKeyDown={(e) => onKeyDown(e, idx)}
            className={cn(
              "inline-flex items-center gap-1.5 rounded px-2.5 py-1 transition-shadow",
              selected ? "shadow-sm" : "",
            )}
            style={{
              background: selected ? "var(--color-paper)" : "transparent",
            }}
          >
            <span
              className="font-serif text-[12.5px]"
              style={{
                color: selected ? "var(--color-ink)" : "var(--color-ink-3)",
              }}
            >
              {option.label}
            </span>
            {option.status === "coming" && (
              <span
                className="font-mono text-[9px] tracking-[0.08em]"
                style={{ color: "var(--color-ink-4)" }}
              >
                SOON
              </span>
            )}
            {option.status === "alpha" && selected && (
              <span
                className="font-mono text-[9px] tracking-[0.08em]"
                style={{ color: "var(--color-ok)" }}
              >
                α
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}
