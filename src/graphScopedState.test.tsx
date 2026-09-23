// GH #543 (audit R9-11/R9-12/R9-13; I-20): state read from one graph never
// shows on another. A graph switch empties every graph-scoped listing before
// the new graph's answers arrive, a late answer from the old graph is dropped,
// and a reopen the backend makes on its own revokes the old binding.
import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest } from "./backend";
import { mockBackend } from "./mock";
import { applyGraphReopened, loadGraphPath } from "./graph";
import { graphBinding } from "./persistence";
import { conflictObjectFor, conflictQueue, graphEpoch, syncConflicts, vcsMarkerConflictFor } from "./ui";
import { loadFavoritesLayout, resetFavoritesLayout, storedFavoritesLayout } from "./favoritesStore";
import { layoutMembers } from "./favoritesLayout";

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

const settle = () => new Promise((r) => setTimeout(r, 20));

afterEach(() => {
  resetFavoritesLayout();
  vi.restoreAllMocks();
});

describe("graph-scoped state across a graph switch", () => {
  it("does not show graph A's conflicts on graph B, pending or failed", async () => {
    const api = mockBackend();
    const base = await api.loadGraph("/g/A");
    let n = 0;
    vi.spyOn(api, "loadGraph").mockImplementation((async (path: string) => {
      n += 1;
      return { ...base, binding_generation: n, application_page_admission: { binding_generation: n },
        meta: { ...(base as { meta: object }).meta, root: path } };
    }) as never);
    const today = "journals/2026_09_23.md";
    const aInventory = {
      sync_conflicts: [{ path: "pages/x.sync-conflict-1.md", base_name: "x", base_path: null, kind: "page", tag: "t", preview: "" }],
      vcs_markers: [{ path: today, name: "Sep 23rd, 2026", kind: "journal", markers: ["<<<<<<<"] }],
      queue: [{ id: "vcs:" + today, source: "vcs-markers", page_name: "Sep 23rd, 2026", page_path: today, kind: "journal", sides: [] }],
    };
    const bInventory = deferred<unknown>();
    vi.spyOn(api, "conflictInventory")
      .mockResolvedValueOnce(aInventory as never)
      .mockReturnValueOnce(bInventory.promise as never);
    __setBackendForTest(api);
    await loadGraphPath("/g/A", { transitionHeld: true });
    await settle();
    expect(vcsMarkerConflictFor(today)).toBeTruthy();

    await loadGraphPath("/g/B", { transitionHeld: true });
    await settle();
    expect(vcsMarkerConflictFor(today)).toBeFalsy();
    expect(conflictObjectFor(today)).toBeFalsy();
    expect(syncConflicts()).toEqual([]);

    bInventory.reject(new Error("stale-graph-binding"));
    await settle();
    expect(vcsMarkerConflictFor(today)).toBeFalsy();
    expect(syncConflicts()).toEqual([]);
    expect(conflictQueue().filter((c) => c.page_path === today)).toEqual([]);
  });

  const favoritesPage = (name: string, members: string[]) => ({
    name, kind: "page", title: name, pre_block: "tine/favorites:: true", rev: `rev-${name}`,
    blocks: members.map((m) => ({ id: m, raw: `[[${m}]]`, collapsed: false, children: [] })),
  });

  it("drops graph A's late favourites arrangement once graph B has its own", async () => {
    const api = mockBackend();
    const aRead = deferred<unknown>();
    vi.spyOn(api, "getPage").mockImplementation(((name: string) =>
      name === "FavA" ? aRead.promise : Promise.resolve(favoritesPage(name, ["B1"]))) as never);
    __setBackendForTest(api);
    const a = loadFavoritesLayout(["A1"], "FavA");
    await loadFavoritesLayout(["B1"], "FavB");
    aRead.resolve(favoritesPage("FavA", ["A1"]));
    await a;
    expect(layoutMembers(storedFavoritesLayout()).map((i) => i.name)).toEqual(["B1"]);
  });

  it("drops graph A's failed favourites read once graph B is flat", async () => {
    const api = mockBackend();
    const aRead = deferred<unknown>();
    vi.spyOn(api, "getPage").mockImplementation(((name: string) =>
      name === "FavA" ? aRead.promise : Promise.resolve(null)) as never);
    __setBackendForTest(api);
    const a = loadFavoritesLayout(["A1"], "FavA");
    await loadFavoritesLayout(["B1"], null);
    aRead.reject(new Error("stale-graph-binding"));
    await a;
    expect(layoutMembers(storedFavoritesLayout()).map((i) => i.name)).toEqual(["B1"]);
  });

  it("revokes the old binding and repaints when the backend reopens the graph", () => {
    const binding = graphBinding();
    const epoch = graphEpoch();
    applyGraphReopened();
    expect(graphBinding()).not.toBe(binding);
    expect(graphEpoch()).toBeGreaterThan(epoch);
  });
});
