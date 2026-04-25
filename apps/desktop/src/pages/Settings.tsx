/**
 * Settings route stub.
 *
 * The real form binds to `heron_read_settings` / `heron_write_settings`
 * via the typed `invoke` wrapper in PR-δ.
 */

import { Link } from "react-router-dom";

export default function Settings() {
  return (
    <main className="p-6 space-y-4">
      <h1 className="text-2xl font-semibold">Settings</h1>
      <p className="text-muted-foreground">
        STT / LLM backend pickers, vault root, hotkey, and the rest of
        the §16.1 form ship in PR-δ.
      </p>
      <Link to="/home" className="underline">
        Back to home
      </Link>
    </main>
  );
}
