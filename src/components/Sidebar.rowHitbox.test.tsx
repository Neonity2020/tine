// GH #464. A left-sidebar row runs the full width of the sidebar; the page name
// in it does not. All of that spare width used to be the link — the hand cursor
// followed the pointer out over empty space, and grabbing a row to reorder it
// opened the page instead of moving it. The link is the title now, and the rest
// of the row is grab space.
//
// This is a render test because what is being asserted is which element answers
// a click and what a drag does to the order — both observable in jsdom. The
// cursor is presentation and is not asserted here.
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { openJournals, resetTabsToJournals, route, tabs } from "../router";
import {
  bumpGraphEpoch,
  closeContextMenu,
  contextMenu,
  favorites,
  resetLeftSidebarSections,
  seedFavorites,
  setFavorites,
  setRecentPages,
} from "../ui";
import type { PageEntry } from "../types";
import { Sidebar } from "./Sidebar";

function rect(left: number, top: number, width: number, height: number): DOMRect {
  return {
    x: left, y: top, width, height, left, top,
    right: left + width, bottom: top + height, toJSON: () => ({}),
  } as DOMRect;
}

function setRect(el: Element | null, left: number, top: number, width: number, height: number) {
  Object.defineProperty(el, "getBoundingClientRect", {
    configurable: true,
    value: () => rect(left, top, width, height),
  });
}

function pointer(type: string, x: number, y: number): PointerEvent {
  const Ctor = (window as { PointerEvent?: typeof PointerEvent }).PointerEvent ?? MouseEvent;
  return new Ctor(type, { bubbles: true, cancelable: true, clientX: x, clientY: y, button: 0, buttons: 1 }) as PointerEvent;
}

/** A real click on the row's blank remainder: the row is the event target,
 *  because there is no child element out there to hit. */
function clickRow(row: HTMLElement) {
  row.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
}

/** A real click on the page title. */
function clickTitle(row: HTMLElement) {
  const label = row.querySelector<HTMLElement>(".nav-page-label");
  expect(label).not.toBeNull();
  label!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
}

afterEach(async () => {
  await new Promise((r) => setTimeout(r, 5));
  closeContextMenu();
  setFavorites([]);
  setRecentPages([]);
  resetLeftSidebarSections();
  resetTabsToJournals();
  openJournals();
  document.body.innerHTML = "";
  localStorage.clear();
  vi.restoreAllMocks();
});

function mount() {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => <Sidebar />, root);
  return { root, dispose };
}

function mountFavorites() {
  seedFavorites(["Alpha", "Beta", "Gamma"]);
  setFavorites([
    { name: "Alpha", kind: "page" },
    { name: "Beta", kind: "page" },
    { name: "Gamma", kind: "page" },
  ]);
  const { root, dispose } = mount();
  const rows = () => [...root.querySelectorAll<HTMLElement>("#sidebar-favorites-list .nav-page")];
  return { root, dispose, rows };
}

describe("left-sidebar row hitbox (GH #464)", () => {
  it("does not navigate when the blank part of a favorite row is clicked, but does from the title", () => {
    const { dispose, rows } = mountFavorites();
    try {
      const beta = rows()[1]!;
      clickRow(beta);
      expect(route()).toMatchObject({ kind: "journals" });
      clickTitle(beta);
      expect(route()).toMatchObject({ kind: "page", name: "Beta" });
    } finally {
      dispose();
    }
  });

  it("does not navigate when the blank part of a Recent row is clicked, but does from the title", () => {
    setRecentPages([{ name: "Recent destination", kind: "page" }]);
    const { root, dispose } = mount();
    try {
      const row = root.querySelector<HTMLElement>("#sidebar-recent-list .nav-page")!;
      clickRow(row);
      expect(route()).toMatchObject({ kind: "journals" });
      clickTitle(row);
      expect(route()).toMatchObject({ kind: "page", name: "Recent destination" });
    } finally {
      dispose();
    }
  });

  it("does not navigate when the blank part of an All pages row is clicked, but does from the title", async () => {
    const pages: PageEntry[] = [{ name: "Listed page", path: "pages/listed-page.md", kind: "page", date_key: null }];
    vi.spyOn(backend(), "listPages").mockResolvedValue(pages);
    bumpGraphEpoch();
    const { root, dispose } = mount();
    try {
      const header = [...root.querySelectorAll<HTMLElement>(".nav-section-header")]
        .find((el) => el.textContent?.includes("ALL PAGES"))!;
      header.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
      const row = await vi.waitFor(() => {
        const found = [...root.querySelectorAll<HTMLElement>(".nav-page")]
          .find((el) => el.textContent?.includes("Listed page"));
        expect(found).toBeDefined();
        return found!;
      });
      clickRow(row);
      expect(route()).toMatchObject({ kind: "journals" });
      clickTitle(row);
      expect(route()).toMatchObject({ kind: "page", name: "Listed page" });
    } finally {
      dispose();
    }
  });

  it("middle-clicking the blank part of a row opens no background tab, but the title still does", () => {
    const { dispose, rows } = mountFavorites();
    try {
      const beta = rows()[1]!;
      const before = tabs().length;
      beta.dispatchEvent(new MouseEvent("auxclick", { bubbles: true, cancelable: true, button: 1 }));
      expect(tabs()).toHaveLength(before);
      beta.querySelector<HTMLElement>(".nav-page-label")!
        .dispatchEvent(new MouseEvent("auxclick", { bubbles: true, cancelable: true, button: 1 }));
      expect(tabs()).toHaveLength(before + 1);
    } finally {
      dispose();
    }
  });

  it("right-clicking anywhere on the row still opens that page's menu", () => {
    const { dispose, rows } = mountFavorites();
    try {
      rows()[1]!.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 10, clientY: 20 }),
      );
      // The row menu is not navigation and cannot be confused with a drag, so it
      // deliberately keeps the whole row.
      expect(contextMenu()).toMatchObject({ name: "Beta" });
    } finally {
      dispose();
    }
  });

  it("dragging a favorite by its title reorders it and does not open it", () => {
    vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const { dispose, rows } = mountFavorites();
    try {
      const [first, second] = rows();
      setRect(first, 0, 0, 200, 30);
      setRect(second, 0, 30, 200, 30);
      const title = first!.querySelector<HTMLElement>(".nav-page-label")!;

      title.dispatchEvent(pointer("pointerdown", 10, 10));
      const prev = document.elementFromPoint;
      try {
        document.elementFromPoint = () => second!;
        document.dispatchEvent(pointer("pointermove", 10, 55));
        document.dispatchEvent(pointer("pointerup", 10, 55));
      } finally {
        document.elementFromPoint = prev;
      }

      expect(favorites().map((f) => f.name)).toEqual(["Beta", "Alpha", "Gamma"]);
      // The click the browser synthesizes on the grabbed title after the drop
      // must not navigate.
      clickTitle(rows()[0]!);
      expect(route()).toMatchObject({ kind: "journals" });
    } finally {
      dispose();
    }
  });
});
