import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings } from "../ui";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
});

describe("Settings maximize (GH #287)", () => {
  it("toggles a transient maximized geometry and restores it, preserving tab and scroll", async () => {
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

  it("does not persist across modal close/reopen", async () => {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);

    openSettings();
    await tick();
    root.querySelector<HTMLButtonElement>(".settings-maximize")!.click();
    await tick();
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(true);

    closeSettings();
    await tick();
    openSettings();
    await tick();
    expect(root.querySelector<HTMLElement>(".modal-overlay")!.classList.contains("settings-maximized")).toBe(false);
    expect(root.querySelector<HTMLButtonElement>(".settings-maximize")!.getAttribute("aria-pressed")).toBe("false");

    dispose();
  });
});
