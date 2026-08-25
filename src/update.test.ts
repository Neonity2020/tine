import { afterEach, describe, expect, it, vi } from "vitest";
import fs from "node:fs";

type Platform = "desktop" | "android" | "ios";

async function loadUpdate(opts: {
  tauri?: boolean;
  platform?: Platform;
  platformReject?: boolean;
  version?: string;
  updaterReject?: Error;
}) {
  vi.resetModules();
  const isTauriMock = vi.fn(() => opts.tauri ?? true);
  const platformKindMock = vi.fn(async (): Promise<Platform> => {
    if (opts.platformReject) throw new Error("platform unavailable");
    return opts.platform ?? "desktop";
  });
  const openExternalMock = vi.fn(async () => {});
  let nextToastId = 40;
  const pushToastMock = vi.fn(() => ++nextToastId);
  const dismissToastMock = vi.fn();
  const getVersionMock = vi.fn(async () => opts.version ?? "0.5.3");
  const updaterCheckMock = opts.updaterReject
    ? vi.fn(async () => { throw opts.updaterReject; })
    : vi.fn(async () => null);

  vi.doMock("./backend", () => ({
    isTauri: isTauriMock,
    backend: () => ({ openExternal: openExternalMock }),
  }));
  vi.doMock("./platform", () => ({ platformKind: platformKindMock }));
  vi.doMock("./ui", () => ({
    pushToast: pushToastMock,
    dismissToast: dismissToastMock,
  }));
  vi.doMock("@tauri-apps/api/app", () => ({ getVersion: getVersionMock }));
  vi.doMock("@tauri-apps/plugin-updater", () => ({ check: updaterCheckMock }));
  vi.doMock("@tauri-apps/plugin-process", () => ({ relaunch: vi.fn(async () => {}) }));

  const update = await import("./update");
  return {
    update,
    platformKindMock,
    getVersionMock,
    updaterCheckMock,
    openExternalMock,
    pushToastMock,
    dismissToastMock,
  };
}

function mockLatest(tag: string, ok = true) {
  const fetchMock = vi.fn(async () => ({
    ok,
    json: async () => ({ tag_name: tag }),
  }));
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

describe("update checks", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it.each(["android", "ios"] as const)("never checks or offers self-update on %s", async (platform) => {
    const fetchMock = mockLatest("v0.6.0");
    const { update, platformKindMock, getVersionMock, updaterCheckMock, openExternalMock, pushToastMock } =
      await loadUpdate({ platform });

    await update.checkForUpdate();
    await expect(update.checkForUpdateNow()).resolves.toEqual({ kind: "unavailable" });

    expect(platformKindMock).toHaveBeenCalled();
    expect(getVersionMock).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(updaterCheckMock).not.toHaveBeenCalled();
    expect(openExternalMock).not.toHaveBeenCalled();
    expect(pushToastMock).not.toHaveBeenCalled();
  });

  it("fails closed when native platform detection fails", async () => {
    const fetchMock = mockLatest("v0.6.0");
    const { update, getVersionMock, updaterCheckMock, pushToastMock } = await loadUpdate({
      platformReject: true,
    });

    await update.checkForUpdate();
    await expect(update.checkForUpdateNow()).resolves.toEqual({ kind: "unavailable" });

    expect(getVersionMock).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(updaterCheckMock).not.toHaveBeenCalled();
    expect(pushToastMock).not.toHaveBeenCalled();
  });

  it("keeps the startup update toast on desktop Tauri", async () => {
    mockLatest("v0.6.0");
    const { update, pushToastMock } = await loadUpdate({ platform: "desktop", version: "0.5.3" });

    await update.checkForUpdate();

    expect(pushToastMock).toHaveBeenCalledWith(
      "Tine 0.6.0 is available — you're on 0.5.3.",
      "info",
      expect.objectContaining({
        sticky: true,
        action: expect.objectContaining({ label: "Install update" }),
      })
    );
  });

  it("keeps the manual current-version result on desktop Tauri", async () => {
    mockLatest("v0.5.3");
    const { update } = await loadUpdate({ platform: "desktop", version: "0.5.3" });

    await expect(update.checkForUpdateNow()).resolves.toEqual({ kind: "current", version: "0.5.3" });
  });

  it("keeps one visible offer when the startup and manual checks find the same update", async () => {
    mockLatest("v0.6.0");
    const { update, pushToastMock, dismissToastMock } = await loadUpdate({
      platform: "desktop",
      version: "0.5.3",
    });

    await update.checkForUpdate();
    await expect(update.checkForUpdateNow()).resolves.toEqual({
      kind: "available",
      version: "0.6.0",
      current: "0.5.3",
    });

    expect(pushToastMock).toHaveBeenCalledTimes(2);
    expect(dismissToastMock).toHaveBeenCalledOnce();
    expect(dismissToastMock).toHaveBeenCalledWith(41);
  });

  it("checks without installing, then surfaces an explicit install failure (GH #241)", async () => {
    mockLatest("v0.6.0");
    const consoleErr = vi.spyOn(console, "error").mockImplementation(() => {});
    const { update, pushToastMock, openExternalMock, updaterCheckMock } = await loadUpdate({
      platform: "desktop",
      version: "0.5.3",
      updaterReject: new Error("minisign signature verification failed"),
    });

    await expect(update.checkForUpdateNow()).resolves.toEqual({
      kind: "available",
      version: "0.6.0",
      current: "0.5.3",
    });
    expect(updaterCheckMock).not.toHaveBeenCalled();
    const toastCalls = pushToastMock.mock.calls as unknown as Array<[
      string,
      string,
      { sticky?: boolean; action?: { label: string; run: () => void } }?,
    ]>;
    const availableToast = toastCalls.find(([message]) =>
      typeof message === "string" && message.includes("0.6.0 is available")
    );
    expect(availableToast?.[2]).toMatchObject({
      sticky: true,
      action: { label: "Install update" },
    });
    availableToast?.[2]?.action?.run();
    await new Promise((r) => setTimeout(r, 10)); // let the detached applyUpdateOrOpen settle

    expect(consoleErr).toHaveBeenCalledWith("[update] self-update failed:", expect.any(Error));
    expect(pushToastMock).toHaveBeenCalledWith(
      expect.stringMatching(/Couldn't apply the update/),
      "error",
    );
    expect(openExternalMock).toHaveBeenCalled(); // releases page still opens as the safe fallback
  });

  it("keeps browser/dev checks inert without probing the native platform", async () => {
    const fetchMock = mockLatest("v0.6.0");
    const { update, platformKindMock } = await loadUpdate({ tauri: false });

    await update.checkForUpdate();
    await expect(update.checkForUpdateNow()).resolves.toEqual({ kind: "unavailable" });

    expect(platformKindMock).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("builds the Windows updater on the OS-native TLS transport (GH #241)", () => {
    const cargo = fs.readFileSync("src-tauri/Cargo.toml", "utf8");
    expect(cargo).toMatch(/cfg\(target_os = "windows"\)[\s\S]*?tauri-plugin-updater\s*=\s*\{[^}]*default-features\s*=\s*false[^}]*"native-tls"/);
    expect(cargo).toMatch(/cfg\(target_os = "windows"\)[\s\S]*?reqwest\s*=\s*\{[^}]*default-features\s*=\s*false[^}]*"system-proxy"/);
    expect(cargo).toMatch(/not\(target_os = "windows"\)[\s\S]*?tauri-plugin-updater\s*=\s*"2"/);
  });
});
