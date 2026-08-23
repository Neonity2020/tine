import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { initParser } from "../render/parse";
import { renderInlines } from "../render/inline";
import { resetStore } from "../store";
import { backend } from "../backend";
import { openPage } from "../router";
import { paneRouter, resetPaneLayoutToSingle } from "../panes";
import { rightSidebar, setRightSidebar, setRightSidebarOpen, setFavorites } from "../ui";
import { Sidebar } from "./Sidebar";
import { NamespaceCrumb } from "./Namespace";
import type { RefGroup } from "../types";

// GH #283 (approved contract): ONE modified-click decision across internal
// page/block link surfaces:
//   plain click   → ordinary navigation;
//   Shift+click   → right sidebar;
//   Ctrl/Cmd(Linux/macOS)+click → background tab;
//   middle-click  → background tab.
// Context menus still expose explicit destinations; text selection / editor
// focus are preserved the same way they always were.

const journalsSnapshot = () => ({
  tabs: [{ history: [{ kind: "journals" as const }], pos: 0, pinned: false }],
  activeIndex: 0,
});

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
  setRightSidebar([]);
  setRightSidebarOpen(false);
  setFavorites([]);
  resetPaneLayoutToSingle(journalsSnapshot());
  localStorage.clear();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

function tabsCount(): number {
  return paneRouter("main").tabs().length;
}

function activeRouteName(): string | null {
  const r = paneRouter("main").route();
  return r.kind === "page" ? r.name : r.kind;
}

function newBackgroundPage(): string | null {
  const tabs = paneRouter("main").tabs();
  const active = activeRouteName();
  const other = tabs.find((t) => {
    const r = t.history[t.pos];
    return r.kind === "page" && r.name !== active;
  });
  return other ? (other.history[other.pos] as { name: string }).name : null;
}

function mountPageRef(): { root: HTMLDivElement; dispose: () => void; anchor: () => HTMLElement } {
  const m = mount(() =>
    renderInlines([{ k: "link", url: { type: "page_ref", v: "Target Page" }, full: "[[Target Page]]" }]) as JSX.Element
  );
  return { ...m, anchor: () => m.root.querySelector<HTMLElement>("a.page-ref")! };
}

function click(el: HTMLElement, init: MouseEventInit = {}) {
  el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, button: 0, ...init }));
}

function auxMiddle(el: HTMLElement) {
  el.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 1 }));
  el.dispatchEvent(new MouseEvent("auxclick", { bubbles: true, cancelable: true, button: 1 }));
}

describe("modified-click contract on internal links (GH #283)", () => {
  it("plain click navigates; Shift+click opens the right sidebar", async () => {
    const { anchor, dispose } = mountPageRef();
    try {
      click(anchor());
      expect(activeRouteName()).toBe("Target Page");
      expect(rightSidebar()).toHaveLength(0);

      openPage("Elsewhere", "page");
      click(anchor(), { shiftKey: true });
      expect(rightSidebar().length).toBeGreaterThan(0);
      const item = rightSidebar()[0];
      expect(item.kind === "page" ? item.name : null).toBe("Target Page");
      expect(activeRouteName()).toBe("Elsewhere");
    } finally {
      dispose();
    }
  });

  it.each([{ ctrlKey: true }, { metaKey: true }])("component-click variant (%s) opens a background tab and never steals focus", async (mod) => {
    const { anchor, dispose } = mountPageRef();
    try {
      const before = tabsCount();
      openPage("Elsewhere", "page");
      click(anchor(), mod);
      expect(tabsCount()).toBe(before + 1);
      expect(activeRouteName()).toBe("Elsewhere");
      expect(newBackgroundPage()).toBe("Target Page");
    } finally {
      dispose();
    }
  });

  it("combined modifiers (shift+ctrl) do not shadow anything — plain click semantics", async () => {
    const { anchor, dispose } = mountPageRef();
    try {
      const before = tabsCount();
      click(anchor(), { shiftKey: true, ctrlKey: true });
      expect(activeRouteName()).toBe("Target Page"); // navigated like a plain click
      expect(tabsCount()).toBe(before);
      expect(rightSidebar()).toHaveLength(0);
    } finally {
      dispose();
    }
  });

  it("middle-click on a page ref opens a background tab (existing convention, asserted)", async () => {
    const { anchor, dispose } = mountPageRef();
    try {
      openPage("Elsewhere", "page");
      const before = tabsCount();
      auxMiddle(anchor());
      expect(tabsCount()).toBe(before + 1);
      expect(activeRouteName()).toBe("Elsewhere");
      expect(newBackgroundPage()).toBe("Target Page");
    } finally {
      dispose();
    }
  });

  it("block refs follow the same rule: Ctrl+click background tab with block anchor, middle too", async () => {
    const group: RefGroup = {
      page: "Anchor Page",
      kind: "page",
      blocks: [{ id: "ref-1", raw: "referenced content", collapsed: false, children: [] }],
    };
    vi.spyOn(backend(), "resolveBlocks").mockImplementation(async (ids) => ids.map((id) => (id === "ref-1" ? group : null)));
    const m = mount(() =>
      renderInlines([{ k: "link", url: { type: "block_ref", v: "ref-1" }, full: "((ref-1))" }]) as JSX.Element
    );
    try {
      const anchor = await vi.waitFor(() => {
        const a = m.root.querySelector<HTMLElement>("span.block-ref");
        expect(a?.textContent).toContain("referenced content");
        return a!;
      });
      const before = tabsCount();
      openPage("Elsewhere", "page");
      anchor.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ctrlKey: true }));
      expect(tabsCount()).toBe(before + 1);
      expect(activeRouteName()).toBe("Elsewhere");
      const bg = paneRouter("main").tabs().find((t) => {
        const r = t.history[t.pos];
        return r.kind === "page" && r.name === "Anchor Page";
      });
      expect(bg).toBeTruthy();
      // middle-click gets the same background treatment.
      const before2 = tabsCount();
      auxMiddle(anchor);
      expect(tabsCount()).toBe(before2 + 1);
      expect(activeRouteName()).toBe("Elsewhere");
    } finally {
      m.dispose();
    }
  });

  it("namespace crumbs follow the same rule (a real link surface", async () => {
    const m = mount(() => <NamespaceCrumb name="a/b/c" />);
    try {
      const crumb = m.root.querySelector<HTMLElement>(".ns-crumb-item");
      expect(crumb).not.toBeNull();
      openPage("Elsewhere", "page");
      const before = tabsCount();
      crumb!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ctrlKey: true }));
      expect(tabsCount()).toBe(before + 1);
      expect(activeRouteName()).toBe("Elsewhere");
      expect(newBackgroundPage()).toBe("a");
    } finally {
      m.dispose();
    }
  });

  it("Favorites rows: Shift→sidebar, Ctrl→background tab", async () => {
    setFavorites([{ name: "Fav One", kind: "page" }]);
    const m = mount(() => <Sidebar />);
    try {
      const row = [...m.root.querySelectorAll<HTMLElement>("#sidebar-favorites-list .nav-page")][0];
      expect(row).toBeTruthy();
      click(row, { shiftKey: true });
      expect(rightSidebar().length).toBeGreaterThan(0);
      setRightSidebar([]);
      const before = tabsCount();
      click(row, { ctrlKey: true });
      expect(tabsCount()).toBe(before + 1);
      expect(newBackgroundPage()).toBe("Fav One");
    } finally {
      m.dispose();
    }
  });
});
