import { useState } from "react";
import { Loader2 } from "lucide-react";

import { Button } from "../../../components/ui/button";
import { TestStatus } from "../../../components/TestStatus";
import { invoke, type TestOutcome } from "../../../lib/invoke";

export function CalendarTab() {
  const [outcome, setOutcome] = useState<TestOutcome | null>(null);
  const [testing, setTesting] = useState(false);

  async function runTest() {
    setTesting(true);
    try {
      const result = await invoke("heron_test_calendar");
      setOutcome(result);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setOutcome({ status: "fail", details: message });
    } finally {
      setTesting(false);
    }
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Calendar</h2>

      <p className="text-sm text-muted-foreground">
        heron reads a one-hour Calendar window when a recording starts
        to attribute the meeting title and attendees. Calendar access
        is read-only and never leaves the device.
      </p>

      <div className="space-y-3">
        <Button
          variant="outline"
          onClick={() => void runTest()}
          disabled={testing}
        >
          {testing ? (
            <>
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
              Testing…
            </>
          ) : (
            "Test calendar access"
          )}
        </Button>
        <TestStatus outcome={outcome} />
      </div>
    </section>
  );
}
