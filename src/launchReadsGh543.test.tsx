// GH #543 (audit R10-06, R10-10), render config: it drives the real
// loadGraphPath. A graph open lists its pages once, and loads the navigation
// index at once and again on a reopen during the launch check.
import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest } from "./backend";
import { mockBackend } from "./mock";
import { loadGraphPath } from "./graph";

const settle = () => new Promise((r) => setTimeout(r, 50));

afterEach(() => vi.restoreAllMocks());

describe("GH #543 graph-open reads", () => {
  // R10-10: two producers answer "which pages does this graph have?" —
  // pages.ts's physicalPagesResource (keyed on graphEpoch + pageInventoryRev)
  // and graph.ts's navigation-index identities (loadAliases after the warm,
  // and App.tsx:1390 on every pageInventoryRev). Each is a whole-page-list
  // IPC, so a graph open lists every page twice and so does every
  // create/delete. R9-08 removed the duplicate inside graph.ts only.
  it("lists the pages once per graph open", async () => {
    const api = mockBackend();
    const list = vi.spyOn(api, "listPages");
    __setBackendForTest(api);
    await import("./pages"); // mounted by the sidebar at startup
    const before = list.mock.calls.length;
    await loadGraphPath("/g/A", { transitionHeld: true });
    await settle();
    await settle();
    const calls = list.mock.calls.length - before;
    expect(calls).toBe(1);
    __setBackendForTest(null);
  });
  // R10-06 and launch design D4 (GH #550): the navigation index loads at
  // graph open, while the launch index check is still running (the backend
  // answers from the index the last session left), so alias links resolve
  // from the first paint. A watcher reopen during the check (config.edn
  // delivered by Syncthing, a `:hidden` edit, a journal-title format change)
  // loads it again for the reopened graph. It used to wait for the check
  // first, and a reopen during that wait left it unloaded until the next save.
  it("loads the navigation index at open and again on a reopen during the launch check", async () => {
    const api = mockBackend();
    vi.spyOn(api, "warmDone").mockImplementation(() => new Promise<boolean>(() => {})); // the check never lands here
    const aliases = vi.spyOn(api, "pageAliases").mockResolvedValue([["Gamma", "Beta"]] as never);
    __setBackendForTest(api);
    const { applyGraphReopened } = await import("./graph");
    const { resolveAlias } = await import("./ui");
    await loadGraphPath("/g/A", { transitionHeld: true });
    await settle();
    expect(resolveAlias("gamma")).toBe("Beta");
    aliases.mockResolvedValue([["Delta", "Beta"]] as never);
    applyGraphReopened(); // graph-rebound from the watcher mid-check
    await settle();
    await settle();
    expect(resolveAlias("delta")).toBe("Beta");
    __setBackendForTest(null);
  });
});
