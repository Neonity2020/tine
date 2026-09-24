// GH #543 (audit R9-08/R9-09/R9-10): the navigation index has two halves --
// alias entries and page identities -- and both belong to one epoch. Each half
// is requested once per epoch, a failed half is asked again, and a repaint
// that moves the epoch while a half is in flight asks again at the new epoch.
import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest } from "./backend";
import { mockBackend } from "./mock";
import { loadNavigationIndex, refreshAliases } from "./graph";
import { aliasMap, bumpGraphEpoch, resolveAlias } from "./ui";

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((res) => { resolve = res; });
  return { promise, resolve };
}

const PAGES = [
  { name: "Alpha", kind: "page", path: "pages/Alpha.md" },
  { name: "Beta", kind: "page", path: "pages/Beta.md" },
];

afterEach(() => vi.restoreAllMocks());

describe("navigation index", () => {
  it("lists the pages once at launch when a save refresh lands before the warm", async () => {
    const api = mockBackend();
    vi.spyOn(api, "pageAliases").mockResolvedValue([["Alpha", "Beta"]] as never);
    const list = vi.spyOn(api, "listPages").mockResolvedValue(PAGES as never);
    __setBackendForTest(api);
    bumpGraphEpoch();
    // A save during the launch pass refreshes the aliases, which completes
    // the index at this epoch...
    await refreshAliases();
    // ...so the warm's own load must not list every page a second time.
    await loadNavigationIndex();
    expect(list).toHaveBeenCalledTimes(1);
    expect(resolveAlias("alpha")).toBe("Alpha");
  });

  it("asks a failed page listing again instead of publishing aliases over no pages", async () => {
    const api = mockBackend();
    vi.spyOn(api, "pageAliases").mockResolvedValue([["Alpha", "Beta"]] as never);
    const list = vi.spyOn(api, "listPages")
      .mockRejectedValueOnce(new Error("the graph was replaced and its replacement was not bound in time"))
      .mockResolvedValue(PAGES as never);
    __setBackendForTest(api);
    bumpGraphEpoch();
    await loadNavigationIndex();
    // The next ordinary save refresh completes the missing half.
    await refreshAliases();
    expect(list).toHaveBeenCalledTimes(2);
    expect(resolveAlias("alpha")).toBe("Alpha");
  });

  it("asks again at the new epoch when a repaint lands while the index is in flight", async () => {
    const api = mockBackend();
    const aliases = deferred<[string, string][]>();
    const pages = deferred<unknown[]>();
    vi.spyOn(api, "pageAliases")
      .mockReturnValueOnce(aliases.promise as never)
      .mockResolvedValue([["Alpha", "Beta"]] as never);
    vi.spyOn(api, "listPages")
      .mockReturnValueOnce(pages.promise as never)
      .mockResolvedValue(PAGES as never);
    __setBackendForTest(api);
    bumpGraphEpoch();
    const load = loadNavigationIndex();
    bumpGraphEpoch(); // a typography change repaints mid-pass
    aliases.resolve([["Alpha", "Beta"]]);
    pages.resolve(PAGES);
    await load;
    await vi.waitFor(() => expect(Object.keys(aliasMap()).length).toBeGreaterThan(0));
    expect(resolveAlias("alpha")).toBe("Alpha");
  });
});
