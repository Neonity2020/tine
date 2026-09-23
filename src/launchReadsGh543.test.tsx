// GH #543 (audit R10-06, R10-10), render config: it drives the real
// loadGraphPath. A graph open lists its pages once, and a reopen during the
// launch warm still loads the navigation index.
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
  // R10-06: a watcher reopen during the launch pass (config.edn delivered by
  // Syncthing, a `:hidden` edit, or — since 0c4c4f52 — any journal-title
  // format change, which now reaches the frontend only as graph-rebound)
  // moves the binding while loadAliases waits for the warm. loadAliases then
  // returns (graph.ts:455-457) and nothing else loads the navigation index
  // until the next save or create/delete: every alias link resolves to a
  // non-existent page meanwhile.
  it("loads the navigation index when the graph is reopened during the launch warm", async () => {
    const api = mockBackend();
    let warmDone!: (v: boolean) => void;
    vi.spyOn(api, "warmDone").mockImplementation(() => new Promise<boolean>((r) => { warmDone = r; }));
    const aliases = vi.spyOn(api, "pageAliases").mockResolvedValue([["Gamma", "Beta"]] as never);
    __setBackendForTest(api);
    const { applyGraphReopened } = await import("./graph");
    const { resolveAlias } = await import("./ui");
    await loadGraphPath("/g/A", { transitionHeld: true });
    await settle();
    const before = aliases.mock.calls.length;
    applyGraphReopened(); // graph-rebound from the watcher mid-pass
    await settle();
    warmDone(true); // the reopened graph's warm completes
    await settle();
    await settle();
    expect(resolveAlias("gamma")).toBe("Beta");
    __setBackendForTest(null);
  });
});

