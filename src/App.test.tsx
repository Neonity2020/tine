import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { handleGraphChange, handleSparseV2Changed, installMobileExternalLinkHandler } from "./App";
import { resetPaneLayoutToSingle, restorePaneLayout } from "./panes";
import { pageByName, reloadPage, resetStore, setDoc, type FeedPage, type Node as StoreNode } from "./store";
import { clearConflict, isConflicted, pageInventoryRev } from "./ui";
import { flushPage, isDirty, markDirty, resetSaveState } from "./persistence";

function addAnchor(href: string): HTMLAnchorElement {
  const a = document.createElement("a");
  a.href = href;
  a.textContent = href;
  document.body.appendChild(a);
  return a;
}

function click(el: Element): MouseEvent {
  const event = new MouseEvent("click", { bubbles: true, cancelable: true });
  el.dispatchEvent(event);
  return event;
}

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
  resetStore();
  resetPaneLayoutToSingle({ tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }], activeIndex: 0 });
});

function page(name: string, kind: "page" | "journal", roots: string[]): FeedPage {
  return { name, kind, title: name, preBlock: null, roots, format: "md", readOnly: false, guide: false };
}

function node(id: string, pageName: string): StoreNode {
  return { id, raw: "loaded elsewhere", collapsed: false, parent: null, page: pageName, children: [] };
}

describe("mobile external link delegation", () => {
  it("opens external links through the OS browser on Android", async () => {
    vi.spyOn(backend(), "appPlatform").mockResolvedValue("android");
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue();
    const uninstall = await installMobileExternalLinkHandler();
    try {
      const a = addAnchor("https://x.test/path");
      const targetClick = vi.fn();
      a.addEventListener("click", targetClick);

      const event = click(a);

      expect(event.defaultPrevented).toBe(true);
      expect(targetClick).not.toHaveBeenCalled();
      expect(openExternal).toHaveBeenCalledTimes(1);
      expect(openExternal).toHaveBeenCalledWith("https://x.test/path");
    } finally {
      uninstall();
    }
  });

  it("does not intercept external links on desktop", async () => {
    vi.spyOn(backend(), "appPlatform").mockResolvedValue("desktop");
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue();
    const uninstall = await installMobileExternalLinkHandler();
    try {
      const a = addAnchor("https://x.test/path");
      a.target = "_blank";
      const event = click(a);

      expect(event.defaultPrevented).toBe(false);
      expect(openExternal).not.toHaveBeenCalled();
    } finally {
      uninstall();
    }
  });

  it("leaves internal hash links untouched on Android", async () => {
    vi.spyOn(backend(), "appPlatform").mockResolvedValue("android");
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue();
    const uninstall = await installMobileExternalLinkHandler();
    try {
      const event = click(addAnchor("#x"));

      expect(event.defaultPrevented).toBe(false);
      expect(openExternal).not.toHaveBeenCalled();
    } finally {
      uninstall();
    }
  });
});

describe("journal watcher feed reconciliation", () => {
  it("restarts a live Journals feed when the changed journal was already loaded in another pane", async () => {
    const name = "15th July, 2030";
    restorePaneLayout(
      { kind: "split", dir: "row", ratio: 0.5, children: [{ kind: "pane", paneId: "main" }, { kind: "pane", paneId: "pane-2" }] },
      new Map([
        ["main", { tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }], activeIndex: 0 }],
        ["pane-2", { tabs: [{ history: [{ kind: "page", name, pageKind: "journal" }], pos: 0, pinned: false }], activeIndex: 0 }],
      ]),
      "main"
    );
    setDoc({ byId: { loaded: node("loaded", name) }, pages: [page(name, "journal", ["loaded"])], feed: [], loaded: true });
    vi.spyOn(backend(), "getPage").mockResolvedValue({ name, kind: "journal", title: name, pre_block: null, blocks: [] });
    const now = new Date();
    const feed = vi.spyOn(backend(), "journalFeedPage").mockResolvedValue({
      pages: [], next_before_day: null, done: true,
      as_of_day: now.getFullYear() * 10_000 + (now.getMonth() + 1) * 100 + now.getDate(),
    });

    await handleGraphChange({ name, kind: "journal", created: false, removed: false });
    await Promise.resolve();
    expect(feed).toHaveBeenCalledTimes(1);
    expect(feed).toHaveBeenCalledWith(3, null);
  });
});

describe("watcher page inventory invalidation", () => {
  it("bumps the rare page-inventory revision for an external create", async () => {
    const before = pageInventoryRev();
    await handleGraphChange({ name: "Created Elsewhere", kind: "page", created: true, removed: false });
    expect(pageInventoryRev()).toBeGreaterThan(before);
  });
});

describe("managed watcher reconciliation", () => {
  it("reloads a visible page and invalidates inventory after an admitted aggregate change", async () => {
    const name = "Managed External";
    resetPaneLayoutToSingle({
      tabs: [{ history: [{ kind: "page", name, pageKind: "page" }], pos: 0, pinned: false }],
      activeIndex: 0,
    });
    setDoc({ byId: { old: node("old", name) }, pages: [page(name, "page", ["old"])], feed: [name], loaded: true });
    const getPage = vi.spyOn(backend(), "getPage").mockResolvedValue({
      name,
      kind: "page",
      title: name,
      pre_block: null,
      blocks: [
        { id: "one", raw: "accepted first", collapsed: false, children: [], breadcrumb: [] },
        { id: "two", raw: "accepted external", collapsed: false, children: [], breadcrumb: [] },
      ],
    });
    const before = pageInventoryRev();

    await handleSparseV2Changed();

    expect(getPage).toHaveBeenCalledWith(name, "page");
    expect(pageInventoryRev()).toBeGreaterThan(before);
    expect(pageByName(name)?.roots).toEqual(["one", "two"]);
  });
});

// A change notification is not per-page divergence evidence. The managed runtime's
// `sparse-v2-changed` tick is a bare aggregate epoch (no page, no origin — it no
// longer fires for an admission that committed nothing, but which page and whose
// write it was are still unknown); the legacy watcher event names a page but
// still cannot tell our own write's echo from someone else's. Declaring a conflict
// on the notification alone blocks `doSave` for that page, so the banner's claim —
// "your unsaved changes weren't written" — comes true only BECAUSE of the banner.
// These cover both directions: a false conflict must not appear, and a REAL external
// change must still surface one.
describe("conflict requires per-page divergence, not just a notification", () => {
  const name = "Managed Racing Edit";

  function liveDirtyPage(rev: string | null) {
    resetPaneLayoutToSingle({
      tabs: [{ history: [{ kind: "page", name, pageKind: "page" }], pos: 0, pinned: false }],
      activeIndex: 0,
    });
    setDoc({ byId: {}, pages: [], feed: [], loaded: true });
    // Seed the save baseline exactly as a real load does, then dirty the page:
    // the user typed and the debounced save has not landed yet.
    reloadPage({
      name,
      kind: "page",
      title: name,
      pre_block: null,
      rev,
      blocks: [{ id: "one", raw: "typed by the user", collapsed: false, children: [] }],
    });
    markDirty(name);
  }

  function storedPage(rev: string | null) {
    return {
      name,
      kind: "page" as const,
      title: name,
      pre_block: null,
      rev,
      blocks: [{ id: "one", raw: "stored content", collapsed: false, children: [] }],
    };
  }

  afterEach(() => {
    resetSaveState();
    clearConflict(name);
  });

  it("does not conflict a dirty page when the admitted epoch left it unchanged", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-1"));

    await handleSparseV2Changed();

    expect(isConflicted(name)).toBe(false);
    // The unsaved edit is still live AND still savable — not replaced by the
    // stored copy, and not frozen behind a conflict.
    expect(isDirty(name)).toBe(true);
    expect(pageByName(name)?.roots).toEqual(["one"]);
  });

  it("still conflicts a dirty page when the stored revision genuinely moved", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));

    await handleSparseV2Changed();

    expect(isConflicted(name)).toBe(true);
  });

  it("still conflicts a dirty page whose file was deleted under it", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);

    await handleSparseV2Changed();

    expect(isConflicted(name)).toBe(true);
  });

  it("leaves an in-flight save alone — its own base_rev guard is the authority", async () => {
    liveDirtyPage("rev-1");
    let release: (rev: string) => void = () => {};
    vi.spyOn(backend(), "savePage").mockReturnValue(
      new Promise<string>((resolve) => {
        release = resolve;
      })
    );
    const getPage = vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));
    const saving = flushPage(name); // in flight: not yet durable, baseline not yet advanced

    await handleSparseV2Changed();

    expect(isConflicted(name)).toBe(false);
    expect(getPage).not.toHaveBeenCalled();
    release("rev-2");
    await saving;
  });

  it("does not conflict a dirty page on a legacy watcher echo of our own write", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-1"));

    await handleGraphChange({ name, kind: "page", created: false, removed: false });

    expect(isConflicted(name)).toBe(false);
    expect(isDirty(name)).toBe(true);
  });

  it("still conflicts a dirty page on a legacy watcher event that really changed it", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));

    await handleGraphChange({ name, kind: "page", created: false, removed: false });

    expect(isConflicted(name)).toBe(true);
  });

  // Direct Files data-safety audit, finding 17. Nothing but an explicit user
  // click ever cleared a conflict, and a removal event raises one with no
  // divergence proof at all — so an editor writing by temp+rename, or a
  // mid-delivery Syncthing pass, could park a page behind a banner permanently.
  // The page then refuses every ordinary save, and both buttons are destructive:
  // "Use disk version" discards the edit, "Keep mine" overwrites a file it never
  // needed to. The divergence proof that gates RAISING a conflict decides this
  // too: when the disk provably holds the editor's own baseline again, the
  // banner's claim is false and it must go.
  it("clears a conflict once the file is back and provably matches the baseline", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    await handleGraphChange({ name, kind: "page", created: false, removed: true });
    expect(isConflicted(name)).toBe(true);

    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-1"));
    await handleGraphChange({ name, kind: "page", created: true, removed: false });

    expect(isConflicted(name)).toBe(false);
    // …and the edit that was frozen behind it is savable again, not discarded.
    expect(isDirty(name)).toBe(true);
    expect(pageByName(name)?.roots).toEqual(["one"]);
  });

  it("keeps the conflict when the file comes back with different content", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    await handleGraphChange({ name, kind: "page", created: false, removed: true });
    expect(isConflicted(name)).toBe(true);

    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));
    await handleGraphChange({ name, kind: "page", created: true, removed: false });

    expect(isConflicted(name)).toBe(true);
  });
});
