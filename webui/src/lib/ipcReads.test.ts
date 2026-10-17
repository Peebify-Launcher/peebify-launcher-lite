import { beforeEach, describe, expect, it, vi } from "vitest";

const calls: Array<{ channel: string; args: unknown[] }> = [];
let reply: (channel: string, args: unknown[]) => unknown = () => undefined;

vi.mock("./tauri", () => ({ isTauri: () => true }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (_cmd: string, { channel, args }: { channel: string; args: unknown[] }) => {
    if (channel === "log-message") return { success: true };
    calls.push({ channel, args });
    return reply(channel, args);
  }),
}));

const { getGameConfig, getLibrary, getRunningGameIds } = await import("./ipc");
const { act, call, errorText, ok, rpcAction } = await import("./rpc");
const { useNotificationStore } = await import("../store/notifications");

describe("config and running-game reads", () => {
  beforeEach(() => {
    calls.length = 0;
    reply = () => undefined;
  });

  it("lists running games with one call when the backend sends runningIds", async () => {
    reply = () => ({ success: true, running: true, gameId: "wuwa", runningIds: ["wuwa", "zzz"] });
    await expect(getRunningGameIds(["genshin", "wuwa", "zzz"])).resolves.toEqual(["wuwa", "zzz"]);
    expect(calls).toEqual([{ channel: "get-running-game", args: [] }]);
  });

  it("reports no answer when the reply has no runningIds", async () => {
    reply = () => ({ success: true, running: false });
    await expect(getRunningGameIds(["genshin", "zzz"])).resolves.toBeNull();
    expect(calls).toEqual([{ channel: "get-running-game", args: [] }]);
  });

  it("reads a game's settings in one targeted get-config call", async () => {
    reply = (_c, args) => {
      const keys = args[0] as string[];
      const values: Record<string, unknown> = Object.fromEntries(keys.map((k) => [k, null]));
      values["games.wuwa.autoUpdate"] = false;
      values["games.wuwa.autoUpdateSchedule"] = "weekly";
      values["games.wuwa.launchArgs"] = "-dx11";
      values["games.wuwa.launchViaSteam"] = false;
      return { success: true, values };
    };
    await expect(getGameConfig("wuwa")).resolves.toEqual({
      prefs: { autoUpdate: false, autoUpdateOnStartup: true, autoUpdateSchedule: "weekly" },
      launchArgs: "-dx11",
      customLauncher: "",
      launchViaSteam: false,
    });
    expect(calls).toHaveLength(1);
    expect(calls[0].channel).toBe("get-config");
    expect((calls[0].args[0] as string[]).every((k) => k.startsWith("games.wuwa."))).toBe(true);
  });

  it("reads only the library key for the library", async () => {
    reply = () => ({ success: true, value: { visible: ["wuwa", "wuwa", "nope"], setupComplete: true } });
    await expect(getLibrary()).resolves.toEqual({ visible: ["wuwa"], setupComplete: true });
    expect(calls).toEqual([{ channel: "get-config", args: ["library"] }]);
  });
});

describe("generic call helpers", () => {
  const errors = () => useNotificationStore.getState().items.filter((n) => n.type === "error");

  beforeEach(() => {
    useNotificationStore.setState({ items: [], toasts: [], open: false });
  });

  it("act keeps a successful reply and drops a failed one, telling about the failure", async () => {
    reply = () => ({ success: true, path: "C:/Mods" });
    await expect(act("actions.failed.changeModsFolder", "set-mods-path")).resolves.toEqual({
      success: true,
      path: "C:/Mods",
    });
    reply = () => ({ success: false, error: "Access denied." });
    await expect(act("actions.failed.changeModsFolder", "set-mods-path")).resolves.toBeUndefined();
    expect(errors().map((n) => n.text)).toEqual(["Access denied."]);
  });

  it("act treats a reply without success as a failure, quietly", async () => {
    reply = () => ({ path: "C:/Mods" });
    await expect(act("actions.failed.changeModsFolder", "set-mods-path")).resolves.toBeUndefined();
    expect(errors()).toHaveLength(0);
  });

  it("ok says whether it worked, and a thrown call fails with its message", async () => {
    reply = () => ({ success: true });
    await expect(ok("actions.failed.deleteMod", "delete-mod")).resolves.toBe(true);
    reply = () => {
      throw new Error("Backend gone.");
    };
    await expect(ok("actions.failed.deleteMod", "delete-mod")).resolves.toBe(false);
    expect(errors().map((n) => n.text)).toEqual(["Backend gone."]);
  });

  it("does not tell about a cancelled action or one with no label", async () => {
    reply = () => ({ success: false, cancelled: true });
    await expect(ok("actions.failed.importMod", "import-mod-archive")).resolves.toBe(false);
    reply = () => ({ success: false, error: "Nope." });
    await expect(rpcAction(null, "set-overlay-hotkey")).resolves.toEqual({ success: false, error: "Nope." });
    expect(errors()).toHaveLength(0);
  });

  it("call returns the reply with ok, or the backend's error, or the fallback text", async () => {
    reply = () => ({ success: true, folder: "C:/Shots" });
    await expect(call("overlay-list-captures", "ipc.captures.readFailed")).resolves.toMatchObject({
      ok: true,
      folder: "C:/Shots",
    });
    reply = () => ({ success: false, error: "Disk unplugged." });
    await expect(call("overlay-list-captures", "ipc.captures.readFailed")).resolves.toMatchObject({
      ok: false,
      error: "Disk unplugged.",
    });
    reply = () => undefined;
    const r = await call("overlay-list-captures", "ipc.captures.readFailed");
    expect(r).toMatchObject({ ok: false, error: expect.any(String) });
    expect(r.error).not.toBe("");
    expect(errors()).toHaveLength(0);
  });

  it("errorText reads an Error or a non-empty string", () => {
    expect(errorText(new Error("boom"))).toBe("boom");
    expect(errorText("plain")).toBe("plain");
    expect(errorText("")).toBeUndefined();
    expect(errorText({ message: "not an error" })).toBeUndefined();
  });
});
