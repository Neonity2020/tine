import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { JSX } from "solid-js";

const platform = vi.hoisted(() => ({ mobile: true }));
vi.mock("../nativeChrome", () => ({
  get isMobilePlatform() {
    return platform.mobile;
  },
}));

type Render = typeof import("solid-js/web")["render"];

function mount(render: Render, node: () => JSX.Element) {
  const div = document.createElement("div");
  document.body.appendChild(div);
  const dispose = render(node, div);
  return { div, dispose };
}

function mockVisualViewport() {
  Object.defineProperty(window, "innerHeight", { value: 800, configurable: true });
  Object.defineProperty(window, "visualViewport", {
    value: {
      height: 520,
      offsetTop: 0,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    },
    configurable: true,
  });
}

describe("MobileKeyboardToolbar", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    mockVisualViewport();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    document.body.innerHTML = "";
  });

  it("renders the confirmed scroll strip and pinned hide button on Android", async () => {
    platform.mobile = true;
    const { render } = await import("solid-js/web");
    const bridge = await import("../editorCommandBridge");
    const { MobileKeyboardToolbar } = await import("./MobileKeyboardToolbar");
    const calls: string[] = [];
    const unregister = bridge.registerFocusedEditorCommandBridge({
      blockId: "b1",
      dispatch(command) {
        calls.push(command);
        return true;
      },
      blur() {
        calls.push("blur");
      },
    });
    const { div, dispose } = mount(render, MobileKeyboardToolbar);

    const toolbar = div.querySelector("[data-mobile-keyboard-toolbar]") as HTMLElement | null;
    expect(toolbar).not.toBeNull();
    expect(toolbar?.style.top).toContain("520px");

    const stripButtons = Array.from(
      toolbar!.querySelectorAll<HTMLButtonElement>(".mobile-keyboard-toolbar-strip .mobile-keyboard-toolbar-btn")
    );
    expect(stripButtons.map((button) => button.getAttribute("aria-label"))).toEqual([
      "Outdent",
      "Indent",
      "Move block up",
      "Move block down",
      "Soft newline",
      "TODO",
      "Camera",
      "Voice memo",
      "Undo",
      "Redo",
      "Date picker",
      "Page reference",
      "Block reference",
      "Slash menu",
    ]);

    const hide = toolbar!.querySelector<HTMLButtonElement>(".mobile-keyboard-toolbar-hide");
    expect(hide?.closest(".mobile-keyboard-toolbar-strip")).toBeNull();
    expect(hide?.getAttribute("aria-label")).toBe("Hide keyboard");

    const down = new Event("pointerdown", { bubbles: true, cancelable: true });
    stripButtons[0].dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
    stripButtons[0].click();
    expect(calls).toEqual(["editor/outdent"]);

    const hideDown = new Event("pointerdown", { bubbles: true, cancelable: true });
    hide!.dispatchEvent(hideDown);
    expect(hideDown.defaultPrevented).toBe(true);
    expect(calls).toEqual(["editor/outdent", "blur"]);

    unregister();
    dispose();
  });

  it("consumes the full hide-keyboard gesture instead of touching through (GH #336)", async () => {
    platform.mobile = true;
    const { render } = await import("solid-js/web");
    const bridge = await import("../editorCommandBridge");
    const { MobileKeyboardToolbar } = await import("./MobileKeyboardToolbar");
    const calls: string[] = [];
    const unregister = bridge.registerFocusedEditorCommandBridge({
      blockId: "b1",
      dispatch: () => true,
      blur() {
        calls.push("blur");
      },
    });
    // The "block underneath": anything the trailing synthesized click could hit.
    const probe = document.createElement("button");
    let probeClicks = 0;
    probe.onclick = () => probeClicks++;
    document.body.appendChild(probe);
    const { div, dispose } = mount(render, MobileKeyboardToolbar);
    const toolbarOf = () => div.querySelector("[data-mobile-keyboard-toolbar]");
    expect(toolbarOf()).not.toBeNull();

    // pointerdown blurs (keyboard starts hiding) — unchanged behavior.
    const hide = toolbarOf()!.querySelector<HTMLButtonElement>(".mobile-keyboard-toolbar-hide")!;
    const down = new Event("pointerdown", { bubbles: true, cancelable: true });
    hide.dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
    expect(calls).toEqual(["blur"]);

    // The IME actually closes: viewport grows back to full height. On Android
    // this is exactly when the toolbar used to vanish mid-gesture.
    Object.defineProperty(window, "visualViewport", {
      value: { height: 800, offsetTop: 0, addEventListener: vi.fn(), removeEventListener: vi.fn() },
      configurable: true,
    });
    window.dispatchEvent(new Event("resize"));
    expect(toolbarOf()).not.toBeNull();

    // The gesture's trailing click lands on the still-mounted button and is
    // swallowed there; the page underneath never sees it.
    const hideStill = toolbarOf()!.querySelector<HTMLButtonElement>(".mobile-keyboard-toolbar-hide")!;
    const click = new MouseEvent("click", { bubbles: true, cancelable: true });
    hideStill.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(true);
    expect(calls).toEqual(["blur"]);
    expect(probeClicks).toBe(0);

    // Once the click is consumed, the toolbar retires with the hidden keyboard.
    expect(toolbarOf()).toBeNull();

    probe.remove();
    unregister();
    dispose();
  });

  it("releases a cancelled hide-keyboard gesture after its safety window", async () => {
    platform.mobile = true;
    const { render } = await import("solid-js/web");
    const bridge = await import("../editorCommandBridge");
    const { MobileKeyboardToolbar } = await import("./MobileKeyboardToolbar");
    const unregister = bridge.registerFocusedEditorCommandBridge({
      blockId: "b1",
      dispatch: () => true,
      blur() {},
    });
    const { div, dispose } = mount(render, MobileKeyboardToolbar);
    const toolbarOf = () => div.querySelector("[data-mobile-keyboard-toolbar]");

    const hide = toolbarOf()!.querySelector<HTMLButtonElement>(".mobile-keyboard-toolbar-hide")!;
    vi.useFakeTimers();
    hide.dispatchEvent(new Event("pointerdown", { bubbles: true, cancelable: true }));
    Object.defineProperty(window, "visualViewport", {
      value: { height: 800, offsetTop: 0, addEventListener: vi.fn(), removeEventListener: vi.fn() },
      configurable: true,
    });
    window.dispatchEvent(new Event("resize"));
    expect(toolbarOf()).not.toBeNull();

    // pointercancel / lost events: no click ever arrives, so the safety window
    // retires the gesture instead of leaking a permanently-visible toolbar.
    vi.advanceTimersByTime(800);
    expect(toolbarOf()).toBeNull();
    vi.useRealTimers();

    unregister();
    dispose();
  });

  it("does not render on desktop even with a focused editor bridge", async () => {
    platform.mobile = false;
    const { render } = await import("solid-js/web");
    const bridge = await import("../editorCommandBridge");
    const { MobileKeyboardToolbar } = await import("./MobileKeyboardToolbar");
    const unregister = bridge.registerFocusedEditorCommandBridge({
      blockId: "b1",
      dispatch: () => true,
      blur() {},
    });
    const { div, dispose } = mount(render, MobileKeyboardToolbar);

    expect(div.querySelector("[data-mobile-keyboard-toolbar]")).toBeNull();

    unregister();
    dispose();
  });
});
