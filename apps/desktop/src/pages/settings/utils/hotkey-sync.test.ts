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

import {
  createHotkeySyncController,
  createHotkeyTestController,
} from "./hotkey-sync";

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

  test("stale-bail after successful register rolls back the orphan registration", async () => {
    // Polish review (Gemini critical): without this rollback, rapid
    // hotkey edits leak ghost OS-level registrations — register(A)
    // succeeds for a stale gen but the controller bails without
    // claiming the ref, leaving A registered with the OS but
    // forgotten by the controller.
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

    // B resolves first and becomes the live combo.
    resolveB();
    await p2;

    // A resolves later — gen is stale. The controller MUST roll back
    // the orphan A registration; otherwise the OS keeps A live even
    // though the user only asked for B.
    resolveA();
    await p1;

    expect(c.getRegisteredCombo()).toBe("B");
    // The stale-rollback unregister(A) MUST appear after register(A)
    // resolved. We don't assert exact ordering of the unregister
    // calls vs. each other (B's flow already had its own unregister
    // path), only that A's orphan was cleaned up.
    const aRegisters = calls.filter(
      (c) => c.kind === "register" && c.combo === "A",
    );
    const aUnregisters = calls.filter(
      (c) => c.kind === "unregister" && c.combo === "A",
    );
    expect(aRegisters).toHaveLength(1);
    expect(aUnregisters.length).toBeGreaterThanOrEqual(1);
  });
});

describe("createHotkeyTestController", () => {
  test("fires onResult with the IPC's `free` flag when combo still matches", async () => {
    let currentCombo = "CmdOrCtrl+Shift+R";
    const results: boolean[] = [];
    const c = createHotkeyTestController({
      checkHotkey: async () => true,
      onResult: (free) => results.push(free),
      getCurrentCombo: () => currentCombo,
    });
    await c.runCheck("CmdOrCtrl+Shift+R");
    expect(results).toEqual([true]);
  });

  test("drops the result when the user changes the chord before IPC resolves", async () => {
    // Issue #212 item 7 — the original code called
    // `setConflict("free")` no matter what, so a late response for an
    // abandoned chord poisoned the UI state for the new chord.
    let currentCombo = "CmdOrCtrl+Shift+R";
    let resolveCheck!: (free: boolean) => void;
    const results: boolean[] = [];
    const c = createHotkeyTestController({
      checkHotkey: () =>
        new Promise<boolean>((resolve) => {
          resolveCheck = resolve;
        }),
      onResult: (free) => results.push(free),
      getCurrentCombo: () => currentCombo,
    });
    const p = c.runCheck("CmdOrCtrl+Shift+R");
    // User changes the chord while the IPC is still in flight.
    currentCombo = "CmdOrCtrl+Shift+T";
    resolveCheck(false);
    await p;
    expect(results).toEqual([]);
  });

  test("a newer runCheck supersedes the older even when they resolve out of order", async () => {
    let currentCombo = "B";
    const results: boolean[] = [];
    let resolveA!: (free: boolean) => void;
    let resolveB!: (free: boolean) => void;
    const c = createHotkeyTestController({
      checkHotkey: (combo) =>
        new Promise<boolean>((resolve) => {
          if (combo === "A") resolveA = resolve;
          else resolveB = resolve;
        }),
      onResult: (free) => results.push(free),
      getCurrentCombo: () => currentCombo,
    });

    // Issue runCheck("A") then runCheck("B"); B is the current combo.
    // Even if A's IPC resolves first, the `getCurrentCombo` guard
    // drops it; if A resolved last the generation counter drops it.
    currentCombo = "A";
    const pA = c.runCheck("A");
    currentCombo = "B";
    const pB = c.runCheck("B");

    // A resolves first with `false` (conflict). It must be dropped —
    // the user's current combo is B.
    resolveA(false);
    await pA;
    expect(results).toEqual([]);

    // B resolves with `true` (free). It's the latest combo + latest
    // generation, so it's the result the UI should see.
    resolveB(true);
    await pB;
    expect(results).toEqual([true]);
  });

  test("onError fires only when the captured combo still matches", async () => {
    let currentCombo = "CmdOrCtrl+Shift+R";
    const errors: string[] = [];
    const c = createHotkeyTestController({
      checkHotkey: async () => {
        throw new Error("daemon down");
      },
      onResult: () => {},
      onError: (m) => errors.push(m),
      getCurrentCombo: () => currentCombo,
    });

    // Stale combo path — the user changed the chord before the error
    // surfaced. No toast.
    const p = c.runCheck("CmdOrCtrl+Shift+R");
    currentCombo = "CmdOrCtrl+Shift+T";
    await p;
    expect(errors).toEqual([]);

    // Live combo path — current matches the captured combo, error fires.
    currentCombo = "CmdOrCtrl+Shift+R";
    await c.runCheck("CmdOrCtrl+Shift+R");
    expect(errors).toHaveLength(1);
    expect(errors[0]).toContain("daemon down");
  });
});
