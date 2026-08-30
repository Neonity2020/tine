import { afterEach, describe, expect, it, vi } from "vitest";
import fs from "node:fs";

type Platform = "desktop" | "android" | "ios";
type ToastCall = [
  string,
  string,
  { sticky?: boolean; action?: { label: string; run: () => void } }?,
];

function toastCalls(mock: { mock: { calls: unknown[][] } }): ToastCall[] {
  return mock.mock.calls as unknown as ToastCall[];
}

async function loadUpdate(opts: {
  tauri?: boolean;
  platform?: Platform;
  platformReject?: boolean;
  version?: string;
  architecture?: string;
  architecturePromise?: Promise<string>;
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
  const openSettingsMock = vi.fn();
  const diagnosticFrontendEventMock = vi.fn(async () => {});
  const debugLogMock = vi.fn(async () => {});
  const appArchitectureMock = vi.fn(async () => opts.architecturePromise ?? opts.architecture ?? "x86_64");
  const getVersionMock = vi.fn(async () => opts.version ?? "0.5.3");
  const updaterCheckMock = opts.updaterReject
    ? vi.fn(async () => { throw opts.updaterReject; })
    : vi.fn(async () => null);

  vi.doMock("./backend", () => ({
    isTauri: isTauriMock,
    backend: () => ({
      openExternal: openExternalMock,
      appArchitecture: appArchitectureMock,
      diagnosticFrontendEvent: diagnosticFrontendEventMock,
      debugLog: debugLogMock,
    }),
  }));
  vi.doMock("./platform", () => ({ platformKind: platformKindMock }));
  vi.doMock("./ui", () => ({
    pushToast: pushToastMock,
    dismissToast: dismissToastMock,
    openSettings: openSettingsMock,
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
    openSettingsMock,
    diagnosticFrontendEventMock,
    debugLogMock,
    appArchitectureMock,
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

  it("keeps one visible offer when two checks resolve concurrently", async () => {
    let resolveArchitecture!: (architecture: string) => void;
    const architecturePromise = new Promise<string>((resolve) => { resolveArchitecture = resolve; });
    const { update, pushToastMock, dismissToastMock, appArchitectureMock } = await loadUpdate({
      platform: "desktop",
      version: "0.5.3",
      architecturePromise,
    });

    const first = update.offerUpdate("0.6.0", "0.5.3");
    const second = update.offerUpdate("0.6.0", "0.5.3");
    expect(appArchitectureMock).toHaveBeenCalledTimes(2);
    resolveArchitecture("x86_64");
    await expect(first).resolves.toBeUndefined();
    await expect(second).resolves.toBeUndefined();

    expect(pushToastMock).toHaveBeenCalledOnce();
    expect(dismissToastMock).not.toHaveBeenCalled();
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
    const availableToast = toastCalls(pushToastMock).find(([message]) =>
      typeof message === "string" && message.includes("0.6.0 is available")
    );
    expect(availableToast?.[2]).toMatchObject({
      sticky: true,
      action: { label: "Install update" },
    });
    availableToast?.[2]?.action?.run();
    await vi.waitFor(() => expect(consoleErr).toHaveBeenCalledWith(
      "[update] self-update failed:",
      "minisign signature verification failed",
    ));
    expect(pushToastMock).toHaveBeenCalledWith(
      expect.stringMatching(/Couldn't apply the update/),
      "error",
      expect.objectContaining({ action: expect.objectContaining({ label: "Diagnostics" }) }),
    );
    expect(openExternalMock).toHaveBeenCalled(); // releases page still opens as the safe fallback
  });

  it("does not offer an impossible native install to an unsupported x86 build (GH #241)", async () => {
    mockLatest("v0.6.0");
    const { update, pushToastMock, updaterCheckMock, openExternalMock, diagnosticFrontendEventMock } =
      await loadUpdate({ platform: "desktop", architecture: "x86", version: "0.5.3" });

    await expect(update.checkForUpdateNow()).resolves.toEqual({
      kind: "available",
      version: "0.6.0",
      current: "0.5.3",
    });

    const offer = toastCalls(pushToastMock).find(([message]) =>
      typeof message === "string" && message.includes("32-bit Windows")
    );
    expect(offer?.[2]).toMatchObject({
      sticky: true,
      action: { label: "Download manually" },
    });
    expect(toastCalls(pushToastMock).some(([, , options]) => options?.action?.label === "Install update")).toBe(false);
    expect(updaterCheckMock).not.toHaveBeenCalled();
    expect(diagnosticFrontendEventMock).toHaveBeenCalledWith(
      "updater_failure",
      undefined,
      undefined,
      undefined,
      "target_selection",
      "unsupported_target",
    );

    offer?.[2]?.action?.run();
    expect(openExternalMock).toHaveBeenCalledOnce();
  });

  it("records a bounded stage/cause and sanitizes the opt-in error chain (GH #241)", async () => {
    mockLatest("v0.6.0");
    const nested = new Error(
      "Proxy-Authorization: Basic dXNlcjpwYXNz; api_key=key123 access_token=token456 " +
      "at C:/Users/Reporter/secret.txt (/home/reporter/private/file), //server/private/share",
    );
    const failure = new Error(
      "error sending request for url https://reporter:hunter2@example.test/latest.json?token=secret",
      { cause: nested },
    );
    const consoleErr = vi.spyOn(console, "error").mockImplementation(() => {});
    const {
      update,
      pushToastMock,
      diagnosticFrontendEventMock,
      debugLogMock,
      openSettingsMock,
    } = await loadUpdate({ platform: "desktop", updaterReject: failure });

    await update.checkForUpdateNow();
    const offer = toastCalls(pushToastMock).find(([, , options]) => options?.action?.label === "Install update");
    offer?.[2]?.action?.run();
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(diagnosticFrontendEventMock).toHaveBeenCalledWith(
      "updater_failure",
      undefined,
      undefined,
      undefined,
      "manifest_fetch",
      "network",
    );
    const debugCalls = debugLogMock.mock.calls as unknown as Array<[string]>;
    const debugLine = String(debugCalls.at(-1)?.[0] ?? "");
    expect(debugLine).toContain("stage=manifest_fetch cause=network");
    expect(debugLine).not.toMatch(
      /dXNlcjpwYXNz|key123|token456|reporter|token=secret|C:[\\/]Users|example\.test|server\/private/,
    );
    expect(String(consoleErr.mock.calls.at(-1)?.[1] ?? "")).not.toMatch(
      /dXNlcjpwYXNz|key123|token456|reporter|token=secret|C:[\\/]Users|example\.test|server\/private/,
    );

    const failureToast = toastCalls(pushToastMock).find(([message]) =>
      typeof message === "string" && message.includes("manifest fetch")
    );
    expect(failureToast?.[2]).toMatchObject({ action: { label: "Diagnostics" } });
    failureToast?.[2]?.action?.run();
    expect(openSettingsMock).toHaveBeenCalledWith("diagnostics");
  });

  it.each([
    ["check", "error sending request for url https://example.test/latest.json", "manifest_fetch", "network"],
    ["check", "failed to deserialize update response", "manifest_parse", "invalid_manifest"],
    ["check", "missing field `version` at line 1 column 17", "manifest_parse", "invalid_manifest"],
    ["check", "None of the fallback platforms were found", "target_selection", "unsupported_target"],
    ["check", "connection failed because the target machine actively refused it", "manifest_fetch", "network"],
    ["apply", "download failed: connection reset", "download", "network"],
    ["apply", "minisign signature verification failed", "signature_verification", "invalid_signature"],
    ["apply", "Failed to install package", "install", "install_failed"],
    ["relaunch", "process restart refused", "relaunch", "relaunch_failed"],
  ] as const)(
    "classifies %s failures without retaining their free-form text",
    async (phase, message, stage, cause) => {
      const { update } = await loadUpdate({ platform: "desktop" });
      expect(update.classifyUpdaterFailure(phase, new Error(message))).toEqual({ stage, cause });
    },
  );

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
