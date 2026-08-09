import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { handleGraphChange, handleSparseV2Changed, installMobileExternalLinkHandler } from "./App";
import { resetPaneLayoutToSingle, restorePaneLayout } from "./panes";
import { pageByName, reloadPage, resetStore, setDoc, type FeedPage, type Node as StoreNode } from "./store";
import { clearConflict, isConflicted, pageInventoryRev } from "./ui";
import { flushPage, forceSave, isDirty, markDirty, resetSaveState } from "./persistence";

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

  /** Stand in for the real Direct backend refusing a guarded save over a file
   *  that moved: it mints an observation epoch for the state it refused to
   *  overwrite and returns it as `conflict:<n>`. Returns the recorded calls so a
   *  test can inspect what a later force presented. */
  function refusingBackend(epoch: number) {
    const calls: Array<{ force: boolean; observation: number | null }> = [];
    vi.spyOn(backend(), "savePage").mockImplementation(
      (_dto, _baseRev, force, observation) => {
        calls.push({ force: !!force, observation: observation ?? null });
        return Promise.reject(new Error(`conflict:${epoch}`));
      }
    );
    return calls;
  }

  /** The authority half of the real backend, which the flat mock above cannot
   *  express: a token exists for exactly one observation, every read or external
   *  observation revokes it, and a refusal mints the next one. `observe()` is
   *  what the native watcher does before its event reaches us. */
  function authoritativeBackend() {
    let live: number | null = null;
    let next = 11;
    const calls: Array<{ force: boolean; observation: number | null }> = [];
    const observe = () => {
      live = null;
    };
    vi.spyOn(backend(), "savePage").mockImplementation(
      (_dto, _baseRev, force, observation) => {
        calls.push({ force: !!force, observation: observation ?? null });
        if (force) {
          if (observation === null || observation === undefined || observation !== live) {
            return Promise.reject(new Error("conflict override authority is missing"));
          }
          live = null;
          return Promise.resolve("rev-forced");
        }
        live = next;
        next += 1;
        return Promise.reject(new Error(`conflict:${live}`));
      }
    );
    return { calls, observe };
  }

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
    refusingBackend(7);

    await handleSparseV2Changed();

    expect(isConflicted(name)).toBe(true);
  });

  it("still conflicts a dirty page whose file was deleted under it", async () => {
    liveDirtyPage("rev-1");
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    refusingBackend(7);

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
    refusingBackend(7);

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
    refusingBackend(7);
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
    refusingBackend(7);
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    await handleGraphChange({ name, kind: "page", created: false, removed: true });
    expect(isConflicted(name)).toBe(true);

    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));
    await handleGraphChange({ name, kind: "page", created: true, removed: false });

    expect(isConflicted(name)).toBe(true);
  });

  // GH #254 increment 2, correction-delta re-verification, HIGH blocker.
  //
  // A banner is only worth raising if its "Keep mine" can work. That force must
  // present the observation epoch it was shown, and only a backend refusal mints
  // one — so a banner the frontend raised by itself presents null, `save_page`
  // refuses it, the retry is forbidden while the page is conflicted, and the one
  // remaining action discards the user's edit. Every path that notices an
  // external change under an unsaved edit therefore goes through the guarded
  // save, whatever noticed it.
  for (const [label, notice] of [
    ["a legacy watcher event", () => handleGraphChange({ name, kind: "page" as const, created: false, removed: false })],
    ["a sparse-v2 epoch", () => handleSparseV2Changed()],
    ["an external delete", () => handleGraphChange({ name, kind: "page" as const, created: false, removed: true })],
  ] as const) {
    it(`gives "Keep mine" real authority after ${label}`, async () => {
      liveDirtyPage("rev-1");
      vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));
      const calls = refusingBackend(11);

      await notice();

      expect(isConflicted(name)).toBe(true);
      // The banner came from a guarded save the backend refused — not from the
      // frontend deciding on its own.
      expect(calls).toEqual([{ force: false, observation: null }]);

      await forceSave(name);

      expect(calls[1]).toEqual({ force: true, observation: 11 });
    });
  }

  // The same blocker one observation later (GH #254 increment 2, SECOND
  // correction-delta re-verification). A banner is up and holding epoch 11 when
  // a second external writer lands. The native watcher revokes that token before
  // its event even reaches us, and the read this handler does to prove divergence
  // revokes again — so by the time the user clicks "Keep mine", epoch 11 names
  // nothing. The page is conflicted and no longer dirty, which is exactly the
  // state an ordinary save declines, so nothing minted a replacement: the banner
  // was permanently disarmed and only the destructive button still worked.
  //
  // Scope note: the sparse-v2 case above models the composition, not a
  // production journey. The managed command maps divergence to a non-banner
  // `managed.conflict` and refuses force outright, which is a separate open
  // limitation rather than something these tests cover.
  it("re-arms the banner when a second external change lands under it", async () => {
    liveDirtyPage("rev-1");
    const { calls, observe } = authoritativeBackend();
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));

    observe(); // the watcher sees winner A
    await handleGraphChange({ name, kind: "page", created: false, removed: false });
    expect(isConflicted(name)).toBe(true);
    expect(isDirty(name)).toBe(false); // the refused save took it out of `dirty`

    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-3"));
    observe(); // winner B: the token behind the visible banner is now dead
    await handleGraphChange({ name, kind: "page", created: false, removed: false });

    expect(isConflicted(name)).toBe(true);
    // The re-observation reached the backend even though the page was clean and
    // already conflicted, and it was NOT a force.
    expect(calls).toEqual([
      { force: false, observation: null },
      { force: false, observation: null },
    ]);

    expect(await forceSave(name)).toBe(true);
    expect(calls[2]).toEqual({ force: true, observation: 12 });
  });

  // The other side of letting a re-observation write: when the disk has come
  // back to something the guard accepts, the edit lands and the banner must not
  // survive its own resolution.
  it("clears the banner when the re-observation's save succeeds", async () => {
    liveDirtyPage("rev-1");
    const { observe } = authoritativeBackend();
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-2"));

    observe();
    await handleGraphChange({ name, kind: "page", created: false, removed: false });
    expect(isConflicted(name)).toBe(true);

    vi.spyOn(backend(), "savePage").mockResolvedValue("rev-4");
    vi.spyOn(backend(), "getPage").mockResolvedValue(storedPage("rev-3"));
    await handleGraphChange({ name, kind: "page", created: false, removed: false });

    expect(isConflicted(name)).toBe(false);
    expect(isDirty(name)).toBe(false);
  });
});
