// GH #543, audit R12-06: a query block's readiness retry of `query_parse`
// belongs to the block and to the graph it was asked of.
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { __setBackendForTest, backend, QueryNotReadyError } from "./backend";
import { mockBackend } from "./mock";
import { initParser } from "./render/parse";
import { QueryMacro } from "./components/Macro";
import { bumpGraphBinding } from "./persistence";

beforeAll(async () => {
  await initParser();
});
afterEach(() => vi.restoreAllMocks());
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

  it("stops polling query_parse once the graph is rebound", async () => {
    const parse = neverReadyParse();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <QueryMacro body="query (task TODO)" />, root);
    await wait(50);
    expect(parse.mock.calls.length).toBeGreaterThan(0);
    bumpGraphBinding();
    const atRebind = parse.mock.calls.length;
    await wait(1600);
    expect(parse.mock.calls.length - atRebind, "the old graph's parse keeps retrying").toBe(0);
    dispose();
    root.remove();
  });
});
