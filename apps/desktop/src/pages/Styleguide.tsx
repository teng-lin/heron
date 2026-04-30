/**
 * Dev-only design-system preview at `/__styleguide`. Renders the new
 * atoms across light/dark × 4 accents so visual regressions are easy
 * to spot during the warm-parchment port. Excluded from production
 * builds via `import.meta.env.DEV` gating in `App.tsx`.
 */

import { useState } from "react";

import { AskBar } from "../components/home/ask-bar";
import { HeroBand } from "../components/home/hero-band";
import { HomeFooterNote } from "../components/home/footer-note";
import { SpacesStrip } from "../components/home/spaces-strip";
import { Avatar } from "../components/ui/avatar";
import { Button } from "../components/ui/button";
import { HeronMark } from "../components/ui/heron-mark";
import { HeronWordmark } from "../components/ui/heron-wordmark";
import { cn } from "../lib/cn";

type Theme = "light" | "dark";
type Accent = "bronze" | "ink" | "heron" | "sage";

const ACCENTS: Accent[] = ["bronze", "ink", "heron", "sage"];

export default function Styleguide() {
  const [theme, setTheme] = useState<Theme>(() => {
    const current = document.documentElement.dataset.theme;
    return current === "dark" ? "dark" : "light";
  });
  const [accent, setAccent] = useState<Accent>(() => {
    const current = document.documentElement.dataset.accent;
    return (ACCENTS.find((a) => a === current) ?? "bronze") as Accent;
  });

  const applyTheme = (t: Theme) => {
    setTheme(t);
    if (t === "dark") {
      document.documentElement.dataset.theme = "dark";
    } else {
      delete document.documentElement.dataset.theme;
    }
    // Persist for `public/fouc-init.js` to pick up on the next reload.
    try {
      localStorage.setItem("heron:theme", t);
    } catch {
      /* private mode / quota — non-fatal, theme will reset on reload */
    }
  };
  const applyAccent = (a: Accent) => {
    setAccent(a);
    document.documentElement.dataset.accent = a;
    try {
      localStorage.setItem("heron:accent", a);
    } catch {
      /* see applyTheme */
    }
  };

  return (
    <main className="min-h-screen bg-paper p-8 text-ink">
      <header className="mb-8 flex items-center justify-between">
        <HeronWordmark size={20} />
        <div className="flex items-center gap-3 font-mono text-xs uppercase tracking-widest text-ink-3">
          <span>__styleguide</span>
        </div>
      </header>

      <section className="mb-10 rounded-lg border border-rule bg-paper-2 p-4">
        <h2 className="mb-3 font-serif text-lg">Theme &amp; accent</h2>
        <div className="flex flex-wrap items-center gap-6">
          <div className="flex items-center gap-2">
            <span className="font-mono text-xs uppercase tracking-widest text-ink-3">
              theme
            </span>
            <ToggleGroup
              options={[
                { value: "light", label: "Light" },
                { value: "dark", label: "Dark" },
              ]}
              value={theme}
              onChange={(v) => applyTheme(v as Theme)}
            />
          </div>
          <div className="flex items-center gap-2">
            <span className="font-mono text-xs uppercase tracking-widest text-ink-3">
              accent
            </span>
            <ToggleGroup
              options={ACCENTS.map((a) => ({ value: a, label: a }))}
              value={accent}
              onChange={(v) => applyAccent(v as Accent)}
            />
          </div>
        </div>
      </section>

      <Section title="Heron mark">
        <div className="flex items-end gap-8">
          <Sample label="size 14">
            <HeronMark size={14} />
          </Sample>
          <Sample label="size 18 (default)">
            <HeronMark size={18} />
          </Sample>
          <Sample label="size 32">
            <HeronMark size={32} />
          </Sample>
          <Sample label="size 64 (accent)">
            <HeronMark size={64} color="var(--color-accent)" />
          </Sample>
        </div>
      </Section>

      <Section title="Heron wordmark">
        <div className="flex flex-col gap-4">
          <Sample label="default 16">
            <HeronWordmark />
          </Sample>
          <Sample label="size 24">
            <HeronWordmark size={24} />
          </Sample>
          <Sample label="size 32">
            <HeronWordmark size={32} />
          </Sample>
        </div>
      </Section>

      <Section title="Avatar">
        <div className="flex items-center gap-3">
          {[
            "Alex Chen",
            "Priya Patel",
            "Sam Okafor",
            "You",
            "Mira Hassan",
            "T",
          ].map((n) => (
            <Sample key={n} label={n}>
              <Avatar name={n} />
            </Sample>
          ))}
        </div>
        <div className="mt-4 flex items-center gap-3">
          {[16, 22, 32, 48].map((s) => (
            <Sample key={s} label={`size ${s}`}>
              <Avatar name="Heron Bird" size={s} />
            </Sample>
          ))}
        </div>
      </Section>

      <Section title="Buttons (existing primitives)">
        <div className="flex flex-wrap gap-3">
          <Button>Default</Button>
          <Button variant="destructive">Destructive</Button>
          <Button variant="outline">Outline</Button>
          <Button variant="ghost">Ghost</Button>
          <Button size="sm">Small</Button>
          <Button size="lg">Large</Button>
        </div>
      </Section>

      <Section title="Type">
        <p className="font-serif text-3xl leading-tight">
          A heron, motionless at the marsh edge.
        </p>
        <p className="font-serif text-xl leading-snug text-ink-2">
          Patient, watchful, precise.
        </p>
        <p className="text-sm text-ink-2">
          Body sans — Inter Tight at 14 px / 1.55 line height. Used for
          most copy in the meeting library and review panes.
        </p>
        <p className="font-mono text-xs uppercase tracking-widest text-ink-3">
          Mono / metadata
        </p>
      </Section>

      <Section title="Color tokens">
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          {[
            "paper",
            "paper-2",
            "paper-3",
            "ink",
            "ink-2",
            "ink-3",
            "ink-4",
            "rule",
            "rule-2",
            "accent",
            "accent-2",
            "accent-soft",
            "rec",
            "ok",
            "warn",
          ].map((tok) => (
            <Swatch key={tok} token={tok} />
          ))}
        </div>
      </Section>

      <Section title="Home — composition">
        <p className="mb-4 text-xs" style={{ color: "var(--color-ink-3)" }}>
          The atoms below stack as the new Home page does. ComingUpBand is
          omitted here because it reads the live calendar store; preview it
          on /home with mock data.
        </p>
        <div
          className="overflow-hidden rounded border"
          style={{ borderColor: "var(--color-rule)" }}
        >
          <HeroBand
            meetingsCount={47}
            hoursCaptured={38.2}
            audioUploaded={0}
          />
          <SpacesStrip />
          <div
            className="px-14 py-10 text-center"
            style={{
              background: "var(--color-paper)",
              color: "var(--color-ink-3)",
            }}
          >
            <p className="font-mono text-[10.5px] uppercase tracking-[0.12em]">
              [ meetings table renders here on /home ]
            </p>
          </div>
          <HomeFooterNote />
          <AskBar />
        </div>
      </Section>
    </main>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="mb-10">
      <h2 className="mb-3 font-mono text-xs uppercase tracking-widest text-ink-3">
        {title}
      </h2>
      <div className="rounded-lg border border-rule bg-paper p-6">
        {children}
      </div>
    </section>
  );
}

function Sample({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-2">
      <div className="flex h-12 items-center justify-center">{children}</div>
      <span className="font-mono text-[10px] uppercase tracking-widest text-ink-3">
        {label}
      </span>
    </div>
  );
}

function Swatch({ token }: { token: string }) {
  const cssVar = `var(--color-${token})`;
  return (
    <div className="flex items-center gap-3 rounded border border-rule bg-paper-2 p-3">
      <span
        className="h-9 w-9 rounded border border-rule"
        style={{ background: cssVar }}
      />
      <div className="flex flex-col">
        <span className="font-mono text-xs text-ink-2">{token}</span>
        <span className="font-mono text-[10px] text-ink-3">{cssVar}</span>
      </div>
    </div>
  );
}

function ToggleGroup<T extends string>({
  options,
  value,
  onChange,
}: {
  options: { value: T; label: string }[];
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="inline-flex overflow-hidden rounded border border-rule">
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            type="button"
            key={opt.value}
            onClick={() => onChange(opt.value)}
            className={cn(
              "px-3 py-1 font-mono text-xs uppercase tracking-widest transition-colors",
              active
                ? "bg-accent text-paper"
                : "bg-paper text-ink-3 hover:bg-paper-2",
            )}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}
