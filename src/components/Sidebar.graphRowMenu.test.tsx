import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { closeContextMenu, closeSwitcher, setGraphMeta } from "../ui";
import { ContextMenu } from "./ContextMenu";
import { GraphSwitcher, type GraphNavigationActions } from "./Sidebar";

// The gesture itself, at the DOM boundary the user actually touches. The
// per-row action set is covered without a DOM in Sidebar.graphSwitcher.test.ts;
// what can only regress here is the wiring: that a right-click on a row reaches
// the shared context menu at all, that it suppresses WebKit's native menu, and
// that it does not tear down the switcher underneath itself.
function dispatchContextMenu(element: Element): MouseEvent {
  const event = new MouseEvent("contextmenu", {
    bubbles: true,
    cancelable: true,
    clientX: 40,
    clientY: 60,
  });
  element.dispatchEvent(event);
  return event;
}

function labels(): string[] {
  return [...document.querySelectorAll<HTMLElement>(".ctx-overlay .ctx-item")]
    .map((item) => item.textContent?.trim() ?? "");
}

afterEach(() => {
  closeContextMenu();
  closeSwitcher();
  setGraphMeta(null);
  vi.restoreAllMocks();
  document.body.innerHTML = "";
});

describe("graph switcher row right-click", () => {
  const mount = () => {
    vi.spyOn(backend(), "listKnownGraphs").mockResolvedValue([
      { name: "Known graph", path: "/graphs/known" },
      { name: "Other graph", path: "/graphs/other" },
    ]);
    const actions: GraphNavigationActions = {
      openKnown: vi.fn(async () => ({ kind: "focused_existing" as const })),
      openPicked: vi.fn(async () => ({ kind: "aborted" as const })),
      createNew: vi.fn(async () => ({ kind: "aborted" as const })),
    };
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => (
      <>
        <GraphSwitcher actions={actions} />
        <ContextMenu />
      </>
    ), root);
    return { root, actions, dispose };
  };

  const openSwitcherMenu = async (root: HTMLElement) => {
    root.querySelector<HTMLElement>(".graph-switch-btn")!.click();
    return vi.waitFor(() => {
      const rows = [...root.querySelectorAll<HTMLElement>(".graph-switch-row")];
      expect(rows).toHaveLength(2);
      return rows;
    });
  };

  it("opens the shared action menu, suppresses the native one, and keeps the switcher up", async () => {
    const { root, dispose } = mount();
    try {
      const rows = await openSwitcherMenu(root);

      const event = dispatchContextMenu(rows[1]);
      // Nothing disables WebKit's own context menu globally, so an unhandled
      // contextmenu here would show the browser menu beside ours.
      expect(event.defaultPrevented).toBe(true);

      await vi.waitFor(() => expect(document.querySelector(".ctx-overlay")).not.toBeNull());
      expect(labels()).toContain("Open in a new window");
      // The switcher's backdrop closes it on any click or contextmenu; the row
      // handler must stop the event before it gets there, or the menu would be
      // anchored to a row that no longer exists.
      expect(root.querySelector(".graph-switch-menu")).not.toBeNull();
    } finally {
      dispose();
    }
  });

  it("acts on the row that was right-clicked, not the one that is open", async () => {
    const { root, actions, dispose } = mount();
    try {
      setGraphMeta({ root: "/graphs/known" } as never);
      const rows = await openSwitcherMenu(root);

      dispatchContextMenu(rows[1]);
      await vi.waitFor(() => expect(document.querySelector(".ctx-overlay")).not.toBeNull());
      const newWindow = [...document.querySelectorAll<HTMLElement>(".ctx-overlay .ctx-item")]
        .find((item) => item.textContent?.trim() === "Open in a new window")!;
      expect(newWindow.classList.contains("ctx-disabled")).toBe(false);
      newWindow.click();

      expect(actions.openKnown).toHaveBeenCalledWith("/graphs/other", true);
    } finally {
      dispose();
    }
  });

  it("shows the current graph's own row as already open rather than offering a no-op", async () => {
    const { root, actions, dispose } = mount();
    try {
      setGraphMeta({ root: "/graphs/known" } as never);
      const rows = await openSwitcherMenu(root);

      dispatchContextMenu(rows[0]);
      await vi.waitFor(() => expect(document.querySelector(".ctx-overlay")).not.toBeNull());
      const items = [...document.querySelectorAll<HTMLElement>(".ctx-overlay .ctx-item")];
      const alreadyOpen = items.find((item) =>
        item.textContent?.trim() === "Open in a new window (already open here)")!;
      expect(alreadyOpen.classList.contains("ctx-disabled")).toBe(true);
      alreadyOpen.click();

      expect(actions.openKnown).not.toHaveBeenCalled();
    } finally {
      dispose();
    }
  });
});
