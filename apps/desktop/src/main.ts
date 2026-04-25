import { invoke } from "@tauri-apps/api/core";

const button = document.querySelector<HTMLButtonElement>("#status");
const output = document.querySelector<HTMLPreElement>("#status-output");

if (button && output) {
  button.addEventListener("click", async () => {
    try {
      const status = await invoke<HeronStatus>("heron_status");
      output.textContent = JSON.stringify(status, null, 2);
    } catch (err) {
      output.textContent = `error: ${err instanceof Error ? err.message : String(err)}`;
    }
  });
}

interface HeronStatus {
  version: string;
  fsm_state: string;
  audio_available: boolean;
  ax_backend: string;
}
