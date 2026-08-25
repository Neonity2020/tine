// GH #211: drag-reorder of left-sidebar favorites — within-list reorder only,
// click/middle-click/context-menu preserved, order persisted through the
// existing config.edn :favorites owner.
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { favorites, seedFavorites, setFavorites, setRecentPages } from "../ui";
import { route, openJournals } from "../router";
import { Sidebar } from "./Sidebar";
import { rowReorderClickSuppressed } from "./rowReorder";

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

afterEach(async () => {
  await new Promise((r) => setTimeout(r, 5));
  setFavorites([]);
  setRecentPages([]);
  document.body.innerHTML = "";
  localStorage.clear();
  openJournals();
  vi.restoreAllMocks();
});

function mountThreeFavorites() {
  // The favorites ARRANGEMENT is module state; seeding membership resets it,
  // exactly as opening a graph does. Without this each test would inherit the
  // previous one's group/order arrangement.
  seedFavorites(["Alpha", "Beta", "Gamma"]);
  setFavorites([
    { name: "Alpha", kind: "page" },
    { name: "Beta", kind: "page" },
    { name: "Gamma", kind: "page" },
  ]);
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => <Sidebar />, root);
  const rows = () => [...root.querySelectorAll<HTMLElement>("#sidebar-favorites-list .nav-page")];
  return { root, dispose, rows };
}

describe("favorites drag reorder (GH #211)", () => {
  it("drags the first favorite past the third and persists the new order to the graph config", () => {
    const setFavs = vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const { dispose, rows } = mountThreeFavorites();
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta", "Gamma"]);

    const [first, second, third] = rows();
    setRect(first, 0, 0, 200, 30);
    setRect(second, 0, 30, 200, 30);
    setRect(third, 0, 60, 200, 30);

    first.dispatchEvent(pointer("pointerdown", 10, 10));
    const prev = document.elementFromPoint;
    try {
      document.elementFromPoint = () => third;
      document.dispatchEvent(pointer("pointermove", 10, 85)); // below Gamma midpoint (60+15=75)
      expect(third.classList.contains("row-drop-after")).toBe(true);
      document.dispatchEvent(pointer("pointerup", 10, 85));
    } finally {
      document.elementFromPoint = prev;
    }

    expect(favorites().map((f) => f.name)).toEqual(["Beta", "Gamma", "Alpha"]);
    expect(setFavs).toHaveBeenCalledWith(["Beta", "Gamma", "Alpha"]);
    dispose();
  });

  it("keeps an ordinary click navigating (sub-threshold press does not reorder)", () => {
    const { dispose, rows } = mountThreeFavorites();
    const second = rows()[1]!;
    setRect(second, 0, 30, 200, 30);

    second.dispatchEvent(pointer("pointerdown", 10, 32));
    document.dispatchEvent(pointer("pointermove", 11, 33)); // < 4px
    document.dispatchEvent(pointer("pointerup", 11, 33));

    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(rowReorderClickSuppressed()).toBe(false);
    second.click();
    expect(route()).toMatchObject({ kind: "page", name: "Beta" });
    dispose();
  });

  it("the click that ends a completed drag is swallowed; a later click navigates", async () => {
    const { root, dispose, rows } = mountThreeFavorites();
    const [first, second] = rows();
    setRect(first, 0, 0, 200, 30);
    setRect(second, 0, 30, 200, 30);

    first.dispatchEvent(pointer("pointerdown", 10, 10));
    const prev = document.elementFromPoint;
    document.elementFromPoint = () => second;
    try {
      document.dispatchEvent(pointer("pointermove", 10, 55));
      document.dispatchEvent(pointer("pointerup", 10, 55));
    } finally {
      document.elementFromPoint = prev;
    }
    expect(favorites().map((f) => f.name)).toEqual(["Beta", "Alpha", "Gamma"]);

    // The drop's synthesized click is suppressed — no navigation.
    (root.querySelector("#sidebar-favorites-list .nav-page") as HTMLElement).click();
    expect(route()).toMatchObject({ kind: "journals" });

    await new Promise((r) => setTimeout(r, 5));
    (root.querySelector("#sidebar-favorites-list .nav-page") as HTMLElement).click();
    expect(route()).toMatchObject({ kind: "page", name: "Beta" });
    dispose();
  });
  // Martin, 2026-08-25: dragging a favorite also started a text selection on
  // its title, so the row's label smeared blue under the cursor. The pointer
  // drag is not a text gesture; while it is armed the document must not be
  // selectable, and the state must be released on drop AND on cancel.
  it("suppresses text selection for the duration of a reorder drag", () => {
    vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const { dispose, rows } = mountThreeFavorites();
    const [first, second, third] = rows();
    setRect(first, 0, 0, 200, 30);
    setRect(second, 0, 30, 200, 30);
    setRect(third, 0, 60, 200, 30);

    expect(document.documentElement.classList.contains("row-reorder-dragging")).toBe(false);

    first.dispatchEvent(pointer("pointerdown", 10, 10));
    // Still under the 4px threshold: this is a click, not a drag — leave
    // ordinary text selection alone.
    document.dispatchEvent(pointer("pointermove", 11, 11));
    expect(document.documentElement.classList.contains("row-reorder-dragging")).toBe(false);

    const prev = document.elementFromPoint;
    try {
      document.elementFromPoint = () => third;
      document.dispatchEvent(pointer("pointermove", 10, 85));
      expect(document.documentElement.classList.contains("row-reorder-dragging")).toBe(true);
      document.dispatchEvent(pointer("pointerup", 10, 85));
    } finally {
      document.elementFromPoint = prev;
    }
    expect(document.documentElement.classList.contains("row-reorder-dragging")).toBe(false);
    dispose();
  });

  it("releases the selection lock when the drag is cancelled", () => {
    vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const { dispose, rows } = mountThreeFavorites();
    const [first, , third] = rows();
    setRect(first, 0, 0, 200, 30);
    setRect(third, 0, 60, 200, 30);

    first.dispatchEvent(pointer("pointerdown", 10, 10));
    const prev = document.elementFromPoint;
    try {
      document.elementFromPoint = () => third;
      document.dispatchEvent(pointer("pointermove", 10, 85));
      expect(document.documentElement.classList.contains("row-reorder-dragging")).toBe(true);
      document.dispatchEvent(pointer("pointercancel", 10, 85));
    } finally {
      document.elementFromPoint = prev;
    }
    expect(document.documentElement.classList.contains("row-reorder-dragging")).toBe(false);
    dispose();
  });
});
