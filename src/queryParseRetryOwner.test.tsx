// GH #543, audits R12-06 and R13-01: a query block's readiness retry of
// `query_parse` belongs to the block and to the graph it was asked of, and a
// reopen of that same graph does not end it.
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { __setBackendForTest, backend, QueryNotReadyError } from "./backend";
import { mockBackend } from "./mock";
import { initParser } from "./render/parse";
import { QueryMacro } from "./components/Macro";
import { bumpGraphBinding } from "./persistence";
import { notifyGraphRebound } from "./modeHooks";
import { bumpGraphEpoch, setGraphMeta } from "./ui";

beforeAll(async () => {
  await initParser();
});
afterEach(() => { vi.restoreAllMocks(); setGraphMeta(null); });
const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function neverReadyParse() {
  __setBackendForTest(mockBackend());
  return vi.spyOn(backend(), "parseQuery").mockImplementation(async () => {
    throw new QueryNotReadyError("indexing");
  });
}

describe("a query block's parse retry (GH #543, R12-06)", () => {
  it("stops polling query_parse once the block is gone", async () => {
    const parse = neverReadyParse();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <QueryMacro body="query (task TODO)" />, root);
    await wait(50);
    expect(parse.mock.calls.length).toBeGreaterThan(0);
    dispose();
    const atUnmount = parse.mock.calls.length;
    await wait(1600);
    expect(parse.mock.calls.length - atUnmount, "a removed block keeps retrying query_parse").toBe(0);
    root.remove();
  });

  it("stops polling query_parse once the app switches to another graph", async () => {
    setGraphMeta({ root: "/first" } as never);
    const parse = neverReadyParse();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <QueryMacro body="query (task TODO)" />, root);
    await wait(50);
    expect(parse.mock.calls.length).toBeGreaterThan(0);
    setGraphMeta({ root: "/second" } as never);
    bumpGraphBinding();
    const atSwitch = parse.mock.calls.length;
    await wait(1600);
    expect(parse.mock.calls.length - atSwitch, "the old graph's parse keeps retrying").toBe(0);
    dispose();
    root.remove();
  });

  it("reads its query once the same graph is reopened", async () => {
    setGraphMeta({ root: "/first" } as never);
    __setBackendForTest(mockBackend());
    const real = backend().parseQuery.bind(backend());
    let ready = false;
    const parse = vi.spyOn(backend(), "parseQuery").mockImplementation(async (...args) => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return real(...args);
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <QueryMacro body="query (task TODO)" />, root);
    await wait(50);
    expect(root.querySelector(".query-readiness-status")?.textContent).toContain("Updating query results");
    // What applyGraphReopened does after a config.edn change.
    notifyGraphRebound();
    bumpGraphEpoch();
    ready = true;
    const atReopen = parse.mock.calls.length;
    await wait(1600);
    expect(parse.mock.calls.length - atReopen, "nothing reads the query on the reopened graph").toBeGreaterThan(0);
    expect(root.querySelector(".query-readiness-status"), "the block still says it waits for the index").toBeNull();
    dispose();
    root.remove();
  });
});
