import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest } from "./backend";
import { openConfiguredHomePage } from "./homePage";
import { mockBackend } from "./mock";
import { resetTabsToJournals, route } from "./router";

afterEach(() => {
  __setBackendForTest(null);
  resetTabsToJournals();
});

describe("configured home-page startup", () => {
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
