import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings, setGraphMeta, setGraphTransitioning, setToasts } from "../ui";
import { graphBindingRuntime } from "../graphBindingRuntime";
import { formatJournal, parseJournalWith } from "../journal";
import {
  changeWideContentWidth,
  resetStandardContentWidth,
  standardContentWidth,
  wideContentWidth,
} from "../contentWidth";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  localStorage.clear();
  setToasts([]);
  graphBindingRuntime.clear();
  setGraphTransitioning(false);
  setGraphMeta(null);
  vi.restoreAllMocks();
  vi.useRealTimers();
  resetStandardContentWidth();
  changeWideContentWidth(null);
});

describe("Settings progressive disclosure and search", () => {
  it("offers the three dot-separated weekday journal formats and the date engine round-trips them", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    openSettings("journals");
    await tick();

    const select = [...root.querySelectorAll<HTMLSelectElement>("select")].find((candidate) =>
      [...candidate.options].some((option) => option.value === "MMM do, yyyy")
    );
    expect(select).toBeDefined();

    const formats = ["E, dd.MM.yyyy", "EEE, dd.MM.yyyy", "EEEE, dd.MM.yyyy"];
    expect([...select!.options].map((option) => option.value)).toEqual(expect.arrayContaining(formats));
    const date = new Date(2026, 7, 11);
    for (const format of formats) {
      const title = formatJournal(date, format);
      expect(parseJournalWith(title, format), `${format} <- ${title}`).toEqual({ y: 2026, m: 8, d: 11 });
    }
    dispose();
  });

  it("exposes the accessible three-mode Link autocomplete policy through Settings search", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    openSettings("editor");
    await tick();

    const search = root.querySelector(".settings-search-input") as HTMLInputElement;
    search.value = "link autocomplete default";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await tick();
    expect(root.textContent).toContain("Link autocomplete default");
    const policy = root.querySelector<HTMLSelectElement>('select[aria-label="Link autocomplete default"]');
    expect(policy?.value).toBe("adaptive");
    expect([...policy!.options].map((option) => option.text)).toEqual([
      "OG adaptive", "Prefer existing", "Prefer exactly what I typed",
    ]);
    policy!.value = "typed";
    policy!.dispatchEvent(new Event("change", { bubbles: true }));
    await tick();
    expect(policy!.value).toBe("typed");
    dispose();
  });

  it("reveals an Advanced match across tabs and clearing restores the collapsed state", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    openSettings("appearance");
    await tick();

    const search = root.querySelector(".settings-search-input") as HTMLInputElement;
    search.value = "diagram editors";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await tick();
    const result = root.querySelector(".settings-search-result") as HTMLButtonElement;
    expect(result.textContent).toContain("Files › Advanced");
    result.click();
    await tick();
    const advanced = root.querySelector(".settings-advanced-toggle") as HTMLButtonElement;
    expect(advanced.getAttribute("aria-expanded")).toBe("true");
    expect(root.textContent).toContain("Diagram editors");

    search.value = "";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await tick();
    expect(advanced.getAttribute("aria-expanded")).toBe("false");
    expect(root.textContent).not.toContain("Edit diagram assets in your own installed app");
    dispose();
  });

  it("finds and applies device-local standard and wide page widths", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    openSettings("appearance");
    await tick();

    const search = root.querySelector(".settings-search-input") as HTMLInputElement;
    search.value = "standard page width";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await tick();
    const result = root.querySelector(".settings-search-result") as HTMLButtonElement;
    expect(result.textContent).toContain("Appearance › Advanced");
    result.click();
    await tick();

    const standard = root.querySelector<HTMLInputElement>('input[aria-label="Standard page width in pixels"]')!;
    standard.value = "960";
    standard.dispatchEvent(new Event("change", { bubbles: true }));
    await tick();
    expect(standardContentWidth()).toBe(960);
    expect(localStorage.getItem("logseq-claude.standard-content-width")).toBe("960");

    const wideMode = root.querySelector<HTMLSelectElement>('select[aria-label="Wide page width mode"]')!;
    wideMode.value = "custom";
    wideMode.dispatchEvent(new Event("change", { bubbles: true }));
    await tick();
    expect(wideContentWidth()).toBe(1280);
    expect(root.querySelector('input[aria-label="Wide page width in pixels"]')).not.toBeNull();

    wideMode.value = "fill";
    wideMode.dispatchEvent(new Event("change", { bubbles: true }));
    await tick();
    expect(wideContentWidth()).toBeNull();
    expect(localStorage.getItem("logseq-claude.wide-content-width")).toBeNull();
    dispose();
  });

  it("persists explicit expansion per tab and supports Escape collapse", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    openSettings("editor");
    await tick();
    const button = root.querySelector(".settings-advanced-toggle") as HTMLButtonElement;
    button.click();
    expect(button.getAttribute("aria-expanded")).toBe("true");
    expect(localStorage.getItem("tine.settings.advanced.editor")).toBe("1");
    button.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    await tick();
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(button);
    expect(localStorage.getItem("tine.settings.advanced.editor")).toBe("0");
    dispose();
  });
});
