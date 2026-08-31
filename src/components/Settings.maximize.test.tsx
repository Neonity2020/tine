import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings } from "../ui";
import { backend } from "../backend";
import { initSettingsLayout, setSettingsMaximized, settingsMaximized } from "../settingsLayout";

const MAXIMIZED_KEY = "settings_dialog_maximized";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  // The remembered size is process-wide (GH #427), so put it back.
  setSettingsMaximized(false);
});

describe("Settings maximize (GH #287)", () => {
  it("toggles the maximized geometry and restores it, preserving tab and scroll", async () => {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);

    openSettings();
    await tick();
    const overlay = root.querySelector<HTMLElement>(".modal-overlay")!;
    expect(overlay.classList.contains("settings-maximized")).toBe(false);

    const toggle = root.querySelector<HTMLButtonElement>(".settings-maximize")!;
    expect(toggle.getAttribute("aria-pressed")).toBe("false");
    expect(toggle.getAttribute("aria-label")).toBe("Maximize settings");

    // Switch to a non-default page and give the body a scroll offset.
    const navItems = [...root.querySelectorAll<HTMLButtonElement>(".settings-nav-item")];
    const editor = navItems.find((button) => button.textContent === "Editor")!;
    editor.click();
    await tick();
    const body = root.querySelector<HTMLElement>(".settings-pane-body")!;
    Object.defineProperty(body, "scrollTop", { configurable: true, value: 120, writable: true });

    toggle.click();
    await tick();
    expect(overlay.classList.contains("settings-maximized")).toBe(true);
    expect(toggle.getAttribute("aria-pressed")).toBe("true");
    expect(toggle.getAttribute("aria-label")).toBe("Restore settings size");
    // Selection and scroll position ride through the pure class toggle.
    expect(root.querySelector<HTMLButtonElement>(".settings-nav-item.active")!.textContent).toBe("Editor");
    expect(body.scrollTop).toBe(120);

    toggle.click();
    await tick();
    expect(overlay.classList.contains("settings-maximized")).toBe(false);
    expect(toggle.getAttribute("aria-pressed")).toBe("false");

    dispose();
  });

  // GH #427 reversed #287's original "transient" contract, on the reporter's
  // case: "I never want the small version … having to press the control on
  // every open is a small tax paid several times a day." A test asserting the
  // old behaviour would now be asserting the bug, so it is replaced rather than
  // grandfathered. The default is unchanged for anyone who never presses it.
  it("opens the way it was left, and only after the user asked for it", async () => {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);

    openSettings();
    await tick();
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(false);
    root.querySelector<HTMLButtonElement>(".settings-maximize")!.click();
    await tick();
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(true);

    closeSettings();
    await tick();
    openSettings();
    await tick();
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(true);
    expect(root.querySelector<HTMLButtonElement>(".settings-maximize")!.getAttribute("aria-pressed")).toBe("true");

    // Restoring sticks too — the memory is of the last choice, not a one-way door.
    root.querySelector<HTMLButtonElement>(".settings-maximize")!.click();
    await tick();
    closeSettings();
    await tick();
    openSettings();
    await tick();
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(false);

    dispose();
  });

  it("remembers the choice for the next run of the app, not just the next open", async () => {
    // What the reporter actually asked for: "Persist it in the app state like
    // the pane layout is." Both halves of the round trip, since either one
    // alone would look like it worked while the setting evaporated on restart:
    // pressing the control reaches the device-local store, and startup reads
    // whatever that store holds.
    setSettingsMaximized(true);
    await tick();
    expect(await backend().getAppBool(MAXIMIZED_KEY, false)).toBe(true);

    await backend().setAppBool(MAXIMIZED_KEY, false);
    await initSettingsLayout();
    expect(settingsMaximized()).toBe(false);

    await backend().setAppBool(MAXIMIZED_KEY, true);
    await initSettingsLayout();
    expect(settingsMaximized()).toBe(true);

    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);
    openSettings();
    await tick();
    // Opens wide immediately — not small-then-snap, which is why the state is
    // hydrated at startup rather than when the dialog mounts.
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(true);
    dispose();
  });
});
