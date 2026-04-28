/**
 * Pollux stub page — `/pollux`.
 *
 * Five-step voice-clone setup wizard shell. Buttons are disabled
 * placeholders; consent checkboxes don't persist. The real
 * implementation (HAL plug-in, voice clone, hand-off classifier)
 * ships at week ~22–26 per `docs/heron-implementation.md`.
 *
 * IMPORTANT: Pollux's voice-clone consent is NOT the same as the
 * existing meeting-disclosure ConsentGate. The two surfaces are
 * intentionally separate — meeting disclosure is "did you tell the
 * room?" mid-call, Pollux consent is BIPA / GDPR-shaped biometric
 * consent for the clone itself. Don't merge them.
 */

import { useState } from "react";

import { Button } from "../components/ui/button";

interface Step {
  id: string;
  title: string;
  body: string;
}

const STEPS: Step[] = [
  {
    id: "consent",
    title: "Biometric consent",
    body: "Voice clones use biometric data. We need explicit, revocable consent before any sample leaves your microphone.",
  },
  {
    id: "passage",
    title: "Read a passage",
    body: "Sixty seconds of speech, read at normal cadence. We use this to train the clone — and only this. Nothing else gets sent to the cloning provider.",
  },
  {
    id: "provider",
    title: "Pick a cloning provider",
    body: "ElevenLabs, OpenAI, or local XTTS. Local stays on-device; cloud providers each have their own retention rules.",
  },
  {
    id: "preview",
    title: "Preview the clone",
    body: "Synthesize a few sentences and listen. If the voice doesn't pass your own ear test, re-record before going live.",
  },
  {
    id: "guardrails",
    title: "Set guardrails",
    body: "Configure the hand-off triggers (your name called, decision request, important topic) and the filler the clone uses while you take over.",
  },
];

const READING_PASSAGE = `The heron stands motionless at the edge of the marsh, head tilted, eyes fixed on the slow movement of water. Patience is its native pace. When the moment comes, it strikes — and then the stillness returns, as if nothing had ever changed. Speech is like that, sometimes. The pause before a careful answer matters more than the answer itself.`;

export default function Pollux() {
  const [step, setStep] = useState(0);
  const [consents, setConsents] = useState({
    biometric: false,
    deepfake: false,
    revocable: false,
  });
  const consentReady = Object.values(consents).every(Boolean);

  return (
    <main className="mx-auto w-full max-w-3xl px-8 py-10">
      <ComingSoonBanner />

      <header className="mt-8 mb-6">
        <p
          className="font-mono text-xs uppercase tracking-[0.12em]"
          style={{ color: "var(--color-ink-3)" }}
        >
          Pollux · the immortal twin
        </p>
        <h1
          className="mt-1 font-serif text-[28px] leading-tight"
          style={{ color: "var(--color-ink)", letterSpacing: "-0.02em" }}
        >
          Voice clone setup
        </h1>
      </header>

      <ol className="mb-6 grid grid-cols-5 gap-2">
        {STEPS.map((s, i) => {
          const active = i === step;
          const done = i < step;
          return (
            <li
              key={s.id}
              className="flex flex-col gap-1"
            >
              <span
                className="h-1 rounded"
                style={{
                  background: active || done
                    ? "var(--color-accent)"
                    : "var(--color-rule)",
                }}
              />
              <span
                className="font-mono text-[10px] uppercase tracking-[0.12em]"
                style={{
                  color: active
                    ? "var(--color-ink)"
                    : "var(--color-ink-3)",
                }}
              >
                {i + 1}. {s.title}
              </span>
            </li>
          );
        })}
      </ol>

      <section
        className="rounded border p-6"
        style={{
          background: "var(--color-paper)",
          borderColor: "var(--color-rule)",
        }}
      >
        <h2
          className="font-serif text-xl"
          style={{ color: "var(--color-ink)" }}
        >
          {STEPS[step].title}
        </h2>
        <p
          className="mt-2 text-sm leading-relaxed"
          style={{ color: "var(--color-ink-2)" }}
        >
          {STEPS[step].body}
        </p>

        {STEPS[step].id === "consent" && (
          <div className="mt-5 space-y-3">
            <ConsentRow
              checked={consents.biometric}
              onChange={(v) =>
                setConsents((c) => ({ ...c, biometric: v }))
              }
            >
              I understand my voice is biometric data and I'm explicitly
              consenting to its capture.
            </ConsentRow>
            <ConsentRow
              checked={consents.deepfake}
              onChange={(v) =>
                setConsents((c) => ({ ...c, deepfake: v }))
              }
            >
              I understand the resulting clone can speak as me, including
              in meetings I'm not personally attending.
            </ConsentRow>
            <ConsentRow
              checked={consents.revocable}
              onChange={(v) =>
                setConsents((c) => ({ ...c, revocable: v }))
              }
            >
              I understand I can revoke this consent any time, which deletes
              the clone and any cached samples on the configured provider.
            </ConsentRow>
          </div>
        )}

        {STEPS[step].id === "passage" && (
          <blockquote
            className="mt-5 rounded border p-4 font-serif text-base leading-relaxed"
            style={{
              background: "var(--color-paper-2)",
              borderColor: "var(--color-rule)",
              color: "var(--color-ink-2)",
            }}
          >
            {READING_PASSAGE}
          </blockquote>
        )}
      </section>

      <div className="mt-6 flex items-center justify-between">
        <Button
          variant="ghost"
          onClick={() => setStep((s) => Math.max(0, s - 1))}
          disabled={step === 0}
        >
          Back
        </Button>
        {step < STEPS.length - 1 ? (
          <Button
            onClick={() => setStep((s) => s + 1)}
            disabled={STEPS[step].id === "consent" && !consentReady}
          >
            Next
          </Button>
        ) : (
          <Button disabled title="Pollux is in development — see docs/heron-implementation.md">
            Start cloning
          </Button>
        )}
      </div>
    </main>
  );
}

function ConsentRow({
  checked,
  onChange,
  children,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  children: React.ReactNode;
}) {
  return (
    <label
      className="flex cursor-pointer items-start gap-3 rounded border p-3 text-sm"
      style={{
        background: checked
          ? "var(--color-accent-soft)"
          : "var(--color-paper-2)",
        borderColor: checked
          ? "var(--color-accent)"
          : "var(--color-rule)",
        color: "var(--color-ink-2)",
      }}
    >
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="mt-0.5"
      />
      <span>{children}</span>
    </label>
  );
}

function ComingSoonBanner() {
  return (
    <div
      className="rounded border px-4 py-3"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-warn)",
      }}
    >
      <p
        className="font-mono text-[10px] uppercase tracking-[0.12em]"
        style={{ color: "var(--color-warn)" }}
      >
        In development · week ~22–26
      </p>
      <p
        className="mt-1 text-sm leading-relaxed"
        style={{ color: "var(--color-ink-2)" }}
      >
        Pollux ships the HAL plug-in, voice clone, and hand-off
        classifier. This screen is a UI shell — the consent
        checkboxes don't persist and the "Start cloning" button is
        disabled until the backend lands.
      </p>
    </div>
  );
}
