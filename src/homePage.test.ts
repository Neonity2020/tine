import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest } from "./backend";
import { getHomePageSetting, openConfiguredHomePage } from "./homePage";
import { mockBackend } from "./mock";
import { resetTabsToJournals, route } from "./router";
import { setGraphMeta } from "./ui";

afterEach(() => {
  __setBackendForTest(null);
  setGraphMeta(null);
  resetTabsToJournals();
});

describe("configured home-page startup", () => {
  it("reads the Logseq-compatible graph owner instead of the device-local legacy key", async () => {
    const api = mockBackend();
    const legacy = vi.spyOn(api, "getAppString").mockResolvedValue("Legacy");
    __setBackendForTest(api);
    setGraphMeta({ root: "/graph", default_home: "Directory" } as never);

    await expect(getHomePageSetting("/graph")).resolves.toBe("Directory");
    expect(legacy).not.toHaveBeenCalled();
  });

  it("migrates the legacy device value once and clears it only after the graph write succeeds", async () => {
    const api = mockBackend();
    vi.spyOn(api, "getAppString").mockResolvedValue("Legacy");
    const setDefaultHome = vi.spyOn(api, "setDefaultHome").mockResolvedValue();
    const clearLegacy = vi.spyOn(api, "setAppString").mockResolvedValue();
    __setBackendForTest(api);
    setGraphMeta({ root: "/graph", default_home: null } as never);

    await expect(getHomePageSetting("/graph")).resolves.toBe("Legacy");
    expect(setDefaultHome).toHaveBeenCalledWith("Legacy");
    expect(clearLegacy).toHaveBeenCalledWith("home.page./graph", "");
  });

  it("keeps the legacy fallback when a malformed graph owner refuses migration", async () => {
    const api = mockBackend();
    vi.spyOn(api, "getAppString").mockResolvedValue("Legacy");
    vi.spyOn(api, "setDefaultHome").mockRejectedValue(new Error("invalid config"));
    const clearLegacy = vi.spyOn(api, "setAppString");
    __setBackendForTest(api);
    setGraphMeta({ root: "/graph", default_home: null } as never);

    await expect(getHomePageSetting("/graph")).resolves.toBe("Legacy");
    expect(clearLegacy).not.toHaveBeenCalled();
  });

  it("does not override a newer user route after its page lookup finishes", async () => {
    const backend = mockBackend();
    vi.spyOn(backend, "getAppString").mockResolvedValue("Home");
    let resolvePage!: (value: Awaited<ReturnType<typeof backend.getPage>>) => void;
    vi.spyOn(backend, "getPage").mockImplementation(() => new Promise((resolve) => {
      resolvePage = resolve;
    }));
    __setBackendForTest(backend);
    let current = true;

    const opening = openConfiguredHomePage("/graph", () => current);
    await vi.waitFor(() => expect(resolvePage).toBeTypeOf("function"));
    current = false;
    resolvePage({
      name: "Home",
      kind: "page",
      title: "Home",
      pre_block: null,
      blocks: [],
      read_only: false,
      guide: false,
    });
    await opening;

    expect(route()).toEqual({ kind: "journals" });
  });
});
