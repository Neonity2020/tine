import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PageEntry } from "./types";

const backendMock = vi.hoisted(() => ({
  listPages: vi.fn(),
  referencedPageNames: vi.fn(),
}));
const waitForWarmCache = vi.hoisted(() => vi.fn(async () => true));

vi.mock("./backend", () => ({ backend: () => backendMock }));
vi.mock("./warmCache", () => ({ waitForWarmCache }));

/** The backend answers with a digest and, when it changed, the set itself. */
const refs = (names: string[] | null, digest = 1) => ({ digest, names });

const page = (name: string): PageEntry => ({
  name,
  kind: "page",
  date_key: null,
  path: `pages/${name.replaceAll("/", "___")}.md`,
});

async function loadInventory() {
  const ui = await import("./ui");
  const pages = await import("./pages");
  return { ...ui, ...pages };
}

beforeEach(() => {
  vi.resetModules();
  backendMock.listPages.mockReset();
  backendMock.referencedPageNames.mockReset();
  waitForWarmCache.mockReset();
  waitForWarmCache.mockResolvedValue(true);
});

describe("GH #229 complete page-name inventory", () => {
  it("keeps All Pages physical-only while physical spelling wins its reference-only fold", async () => {
    backendMock.listPages.mockResolvedValue([page("Test")]);
    backendMock.referencedPageNames.mockResolvedValue(refs(["test", "test/testy test"]));
    const { allPageNames, allPages } = await loadInventory();

    await vi.waitFor(() => {
      expect(allPages()).toEqual([page("Test")]);
      expect(allPageNames()).toEqual(["Test", "test/testy test"]);
    });
  });

  it("refreshes only reference names after dataRev", async () => {
    let current = ["test/first"];
    backendMock.listPages.mockResolvedValue([page("test")]);
    backendMock.referencedPageNames.mockImplementation(async () => refs(current, current.length));
    const { allPageNames, bumpDataRev } = await loadInventory();

    await vi.waitFor(() => expect(allPageNames()).toEqual(["test", "test/first"]));
    backendMock.listPages.mockClear();
    backendMock.referencedPageNames.mockClear();
    current = ["test/second", "test/third"];
    bumpDataRev();

    await vi.waitFor(() => expect(allPageNames()).toEqual(["test", "test/second", "test/third"]));
    expect(backendMock.listPages).not.toHaveBeenCalled();
    expect(backendMock.referencedPageNames).toHaveBeenCalledTimes(1);
  });

  it("refreshes physical pages after pageInventoryRev on both create and delete", async () => {
    let physical = [page("test")];
    backendMock.listPages.mockImplementation(async () => physical);
    backendMock.referencedPageNames.mockResolvedValue(refs(["test/linked"]));
    const { allPages, bumpPageInventoryRev } = await loadInventory();

    await vi.waitFor(() => expect(allPages()).toEqual([page("test")]));
    backendMock.listPages.mockClear();
    physical = [page("test"), page("created")];
    bumpPageInventoryRev();
    await vi.waitFor(() => expect(allPages()).toEqual([page("test"), page("created")]));
    expect(backendMock.listPages).toHaveBeenCalledTimes(1);

    backendMock.listPages.mockClear();
    physical = [page("test")];
    bumpPageInventoryRev();
    await vi.waitFor(() => expect(allPages()).toEqual([page("test")]));
    expect(backendMock.listPages).toHaveBeenCalledTimes(1);
  });

  // Direct Files performance audit 2026-08-09, finding F7. This resource re-runs
  // after every typing lull and the answer almost never changes, so the backend
  // may reply with the digest alone. The dangerous reading of that reply is
  // "empty": it would erase every reference-only page from autocomplete and the
  // namespace tree until the next real change. This pins the safe one, and that
  // the digest we were given is actually presented back.
  it("keeps the inventory when the backend says the set is unchanged", async () => {
    backendMock.listPages.mockResolvedValue([page("test")]);
    backendMock.referencedPageNames.mockResolvedValue(refs(["test/linked"], 77));
    const { allPageNames, bumpDataRev } = await loadInventory();

    await vi.waitFor(() => expect(allPageNames()).toEqual(["test", "test/linked"]));

    backendMock.referencedPageNames.mockClear();
    backendMock.referencedPageNames.mockResolvedValue(refs(null, 77));
    bumpDataRev();

    await vi.waitFor(() =>
      expect(backendMock.referencedPageNames).toHaveBeenCalledWith(77)
    );
    expect(allPageNames()).toEqual(["test", "test/linked"]);
  });

  // Harvest W4-P1 item 2 — a COUNTER PROBE, not a necessity test. It reports
  // what the merge memo actually costs across five typing lulls in which the
  // backend answers "unchanged against the digest you carried". The bound below
  // is what HEAD already delivers: `referencedNames` resolves to the SAME array
  // reference, and a Solid resource value is stored in a signal whose default
  // comparator is identity, so the merge memo is never notified. If this ever
  // becomes nonzero, that count is the pre-fix number for a real cut.
  it("rebuilds the page-name merge zero times across five unchanged-reply lulls", async () => {
    const CARRIED_DIGEST = 77;
    const LULLS = 5;
    const classes = { "changed-names": 0, "unchanged-names-null": 0, "request-failed": 0 };
    backendMock.listPages.mockResolvedValue([page("test")]);
    backendMock.referencedPageNames.mockImplementation(async () => {
      classes["changed-names"]++;
      return refs(["test/linked"], CARRIED_DIGEST);
    });
    const { allPageNames, bumpDataRev } = await loadInventory();
    const { __pagesTestHooks } = await import("./pages");

    await vi.waitFor(() => expect(allPageNames()).toEqual(["test", "test/linked"]));
    const settled = allPageNames();

    backendMock.referencedPageNames.mockClear();
    backendMock.referencedPageNames.mockImplementation(async () => {
      classes["unchanged-names-null"]++;
      return refs(null, CARRIED_DIGEST);
    });
    let merges = 0;
    __pagesTestHooks.onMergePageNames = () => {
      merges++;
    };
    for (let lull = 0; lull < LULLS; lull++) {
      bumpDataRev();
      await vi.waitFor(() =>
        expect(backendMock.referencedPageNames).toHaveBeenCalledTimes(lull + 1)
      );
      // Settle the resource write that follows the awaited reply.
      await Promise.resolve();
      await Promise.resolve();
    }
    __pagesTestHooks.onMergePageNames = undefined;

    // eslint-disable-next-line no-console -- the measurement IS the receipt.
    console.log(
      `w4_p1_page_name_merge lulls=${LULLS} merges=${merges} ` +
        `changed-names=${classes["changed-names"]} unchanged-names-null=${classes["unchanged-names-null"]} ` +
        `request-failed=${classes["request-failed"]}`
    );

    // Every lull presented the carried digest, not null.
    expect(backendMock.referencedPageNames.mock.calls).toEqual(
      Array.from({ length: LULLS }, () => [CARRIED_DIGEST])
    );
    expect(allPageNames()).toEqual(settled);
    expect(allPageNames()).toBe(settled);
    expect({ merges, ...classes }).toEqual({
      merges: 0,
      "changed-names": 1,
      "unchanged-names-null": LULLS,
      "request-failed": 0,
    });
  });

  it("rejects physical and reference-name responses from a superseded graph", async () => {
    const physicalResolvers: Array<(pages: PageEntry[]) => void> = [];
    const referenceResolvers: Array<(names: string[]) => void> = [];
    backendMock.listPages.mockImplementation(() => new Promise<PageEntry[]>((resolve) => {
      physicalResolvers.push(resolve);
    }));
    backendMock.referencedPageNames.mockImplementation(() => new Promise<{ digest: number; names: string[] | null }>((resolve) => {
      referenceResolvers.push((names) => resolve(refs(names)));
    }));
    const { allPageNames, bumpGraphEpoch } = await loadInventory();

    await vi.waitFor(() => {
      expect(physicalResolvers).toHaveLength(1);
      expect(referenceResolvers).toHaveLength(1);
    });
    bumpGraphEpoch();
    await vi.waitFor(() => {
      expect(physicalResolvers).toHaveLength(2);
      expect(referenceResolvers).toHaveLength(2);
    });

    physicalResolvers[1]([page("fresh")]);
    referenceResolvers[1](["fresh/child"]);
    await vi.waitFor(() => expect(allPageNames()).toEqual(["fresh", "fresh/child"]));

    physicalResolvers[0]([page("stale")]);
    referenceResolvers[0](["stale/child"]);
    await Promise.resolve();
    await Promise.resolve();
    expect(allPageNames()).toEqual(["fresh", "fresh/child"]);
  });
});
