/**
 * State-machine tests for the Settings → Hotkey OS-registration sync
 * controller. Mirrors the `ActionItemsEditor.test.ts` style: drive the
 * controller factory directly with injectable IPC mocks rather than
 * mounting React. There's no jsdom in this workspace.
 *
 * Cases pinned (issue #212 item 6):
 *
 *   1. Initial sync registers the new combo; nothing to unregister.
 *   2. Sync to a *different* combo registers the new one FIRST, then
 *      unregisters the old. A failed register must not unregister the
 *      previous combo — the user keeps a working hotkey.
 *   3. Sync to `""` unregisters the previous combo and clears the
 *      controller's `registeredCombo`. Without this fix, the OS
 *      hotkey stayed live after the user cleared the field.
 *   4. Sync to the same combo as already registered is a no-op (no
 *      IPC fires).
 *   5. Rapid sync calls (combo A, then combo B before A resolves) —
 *      only the latest generation mutates `registeredCombo`.
 */

import { describe, expect, test } from "bun:test";

import { createHotkeySyncController } from "./hotkey-sync";

interface Call {
  kind: "register" | "unregister";
  combo: string;
}

function makeIpc() {
  const calls: Call[] = [];
  return {
    calls,
    registerHotkey: async (combo: string) => {
      calls.push({ kind: "register", combo });
      return undefined as unknown;
    },
    unregisterHotkey: async (combo: string) => {
      calls.push({ kind: "unregister", combo });
      return undefined as unknown;
    },
  };
}

describe("createHotkeySyncController", () => {
  test("first sync registers the combo without trying to unregister", async () => {
    const ipc = makeIpc();
    const c = createHotkeySyncController({
      registerHotkey: ipc.registerHotkey,
      unregisterHotkey: ipc.unregisterHotkey,
    });
    await c.sync("CmdOrCtrl+Shift+R");
    expect(ipc.calls).toEqual([
      { kind: "register", combo: "CmdOrCtrl+Shift+R" },
    ]);
    expect(c.getRegisteredCombo()).toBe("CmdOrCtrl+Shift+R");
  });

  test("changing combos registers the new one first, then unregisters the old", async () => {
    // Issue #212 item 6 — the previous shape did this in reverse, so
    // a failed register left the user with no working hotkey.
    const ipc = makeIpc();
    const c = createHotkeySyncController({
      registerHotkey: ipc.registerHotkey,
      unregisterHotkey: ipc.unregisterHotkey,
    });
    await c.sync("CmdOrCtrl+Shift+R");
    await c.sync("CmdOrCtrl+Shift+T");
    expect(ipc.calls).toEqual([
      { kind: "register", combo: "CmdOrCtrl+Shift+R" },
      { kind: "register", combo: "CmdOrCtrl+Shift+T" },
      { kind: "unregister", combo: "CmdOrCtrl+Shift+R" },
    ]);
    expect(c.getRegisteredCombo()).toBe("CmdOrCtrl+Shift+T");
  });

  test("failed register leaves the previous combo registered", async () => {
    // Belt-and-suspenders: if `heron_register_hotkey` rejects, the old
    // combo MUST still be live. The controller never reaches the
    // unregister call.
    const calls: Call[] = [];
    const errors: string[] = [];
    let registerSeq = 0;
    const c = createHotkeySyncController({
      registerHotkey: async (combo) => {
        calls.push({ kind: "register", combo });
        registerSeq += 1;
        if (registerSeq === 2) {
          throw new Error("claimed by Finder");
        }
        return undefined;
      },
      unregisterHotkey: async (combo) => {
        calls.push({ kind: "unregister", combo });
        return undefined;
      },
      onError: (m) => errors.push(m),
    });
    await c.sync("CmdOrCtrl+Shift+R");
    expect(c.getRegisteredCombo()).toBe("CmdOrCtrl+Shift+R");
    await c.sync("CmdOrCtrl+Shift+T");
    // Register-then-unregister means a failed register MUST NOT
    // unregister the old combo — the user keeps a working hotkey.
    expect(calls).toEqual([
      { kind: "register", combo: "CmdOrCtrl+Shift+R" },
      { kind: "register", combo: "CmdOrCtrl+Shift+T" },
    ]);
    expect(c.getRegisteredCombo()).toBe("CmdOrCtrl+Shift+R");
    expect(errors).toHaveLength(1);
    expect(errors[0]).toContain("claimed by Finder");
  });

  test("sync to empty string unregisters the previous combo", async () => {
    // Issue #212 item 6 — the previous shape bailed early on `""`,
    // leaving the OS hotkey active after the user cleared the field.
    const ipc = makeIpc();
    const c = createHotkeySyncController({
      registerHotkey: ipc.registerHotkey,
      unregisterHotkey: ipc.unregisterHotkey,
    });
    await c.sync("CmdOrCtrl+Shift+R");
    await c.sync("");
    expect(ipc.calls).toEqual([
      { kind: "register", combo: "CmdOrCtrl+Shift+R" },
      { kind: "unregister", combo: "CmdOrCtrl+Shift+R" },
    ]);
    expect(c.getRegisteredCombo()).toBeNull();
  });

  test("sync to empty string before any registration is a no-op", async () => {
    const ipc = makeIpc();
    const c = createHotkeySyncController({
      registerHotkey: ipc.registerHotkey,
      unregisterHotkey: ipc.unregisterHotkey,
    });
    await c.sync("");
    expect(ipc.calls).toHaveLength(0);
    expect(c.getRegisteredCombo()).toBeNull();
  });

  test("sync to the same combo is a no-op", async () => {
    const ipc = makeIpc();
    const c = createHotkeySyncController({
      registerHotkey: ipc.registerHotkey,
      unregisterHotkey: ipc.unregisterHotkey,
    });
    await c.sync("CmdOrCtrl+Shift+R");
    await c.sync("CmdOrCtrl+Shift+R");
    expect(ipc.calls).toEqual([
      { kind: "register", combo: "CmdOrCtrl+Shift+R" },
    ]);
  });

  test("rapid sync edits drop the stale generation's writes", async () => {
    // Two concurrent sync calls — the second supersedes the first.
    // Without the generation counter, the older sync's late
    // `registeredCombo = A` write could overwrite the newer sync's
    // `registeredCombo = B`.
    const calls: Call[] = [];
    let resolveA!: () => void;
    let resolveB!: () => void;
    const c = createHotkeySyncController({
      registerHotkey: async (combo) => {
        calls.push({ kind: "register", combo });
        return new Promise<void>((resolve) => {
          if (combo === "A") resolveA = resolve;
          else resolveB = resolve;
        });
      },
      unregisterHotkey: async (combo) => {
        calls.push({ kind: "unregister", combo });
        return undefined;
      },
    });

    const p1 = c.sync("A");
    const p2 = c.sync("B");

    // The newer call resolves FIRST; older resolves second.
    resolveB();
    await p2;
    resolveA();
    await p1;

    // The latest sync wins.
    expect(c.getRegisteredCombo()).toBe("B");
  });
});
