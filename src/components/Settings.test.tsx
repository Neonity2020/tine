import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings, setToasts, toasts } from "../ui";
import { backend } from "../backend";
import * as store from "../store";
import type { SparseV2Status } from "../types";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const showSparsePanel = async (root: HTMLElement) => {
  openSettings();
  await tick();
  const backupsTab = [...root.querySelectorAll(".settings-nav-item")].find(
    (button) => button.textContent === "Backups & recovery"
  ) as HTMLButtonElement;
  backupsTab.click();
  await tick();
};

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  localStorage.clear();
  setToasts([]);
  vi.restoreAllMocks();
});

describe("Settings sparse-v2 authority transitions", () => {
  const legacy = (): SparseV2Status => ({
    state: "legacy_default",
    runtime: null,
    can_activate: true,
    can_retry: false,
    can_cancel: false,
    cancel_reason: null,
    binding_generation: 10,
  });

  const localActive = (): SparseV2Status => ({
    state: "active",
    runtime: {
      lifecycle: "active",
      recovery: "first_promotion",
      watcher: {
        latest_enqueue: 0,
        acknowledged: 0,
        drain_in_flight: false,
        pending: false,
        pending_requires_full_scan: false,
        deferred: false,
        quiescing: false,
        sequence_exhausted: false,
      },
      last_tick: null,
      detail: null,
      shared_role: null,
      shared_phase: null,
      provider_pending: 0,
    },
    can_activate: false,
    can_retry: false,
    can_cancel: true,
    cancel_reason: null,
    binding_generation: 11,
  });

  const localRetryable = (): SparseV2Status => ({
    state: "retryable",
    stage: "shadow_import",
    detail: "projection proof paused on the exact test cut",
    runtime: null,
    can_activate: false,
    can_retry: true,
    can_cancel: true,
    cancel_reason: null,
    binding_generation: 11,
  });

  it("flushes before activation, exposes returned failure detail, and invalidates stale pages", async () => {
    const calls: string[] = [];
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockImplementation(async () => {
      calls.push("flush");
      return true;
    });
    const reset = vi.spyOn(store, "resetStore");
    vi.spyOn(backend(), "activateSparseV2").mockImplementation(async () => {
      calls.push("activate");
      return localRetryable();
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const enable = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Enable sparse v2")
    ) as HTMLButtonElement;
    enable.click();
    await tick();
    await tick();

    expect(calls).toEqual(["flush", "activate"]);
    expect(reset).toHaveBeenCalled();
    expect(toasts().at(-1)?.message).toContain(
      "projection proof paused on the exact test cut"
    );
    expect(root.textContent).toContain("Resume point: shadow_import");
    expect(root.textContent).toContain("Return to standard Markdown mode");
    dispose();
  });

  it("retries a failed pre-cancel flush after rollback, then resets under the returned legacy generation", async () => {
    const calls: string[] = [];
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    let flushAttempt = 0;
    vi.spyOn(store, "flushAll").mockImplementation(async () => {
      flushAttempt += 1;
      calls.push(`flush-${flushAttempt}`);
      return flushAttempt === 2;
    });
    const reset = vi.spyOn(store, "resetStore").mockImplementation(() => {
      calls.push("reset");
    });
    vi.spyOn(backend(), "cancelSparseV2").mockImplementation(async () => {
      calls.push("cancel");
      const status = { ...legacy(), binding_generation: 12 };
      return {
        status,
        binding_generation: 12,
        recovery_statement: "Private sparse-v2 recovery state was preserved.",
      };
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to standard Markdown mode")
    ) as HTMLButtonElement;
    expect(rollback).toBeTruthy();
    rollback.click();
    await tick();
    await tick();

    expect(calls).toEqual(["flush-1", "cancel", "flush-2", "reset"]);
    expect(reset).toHaveBeenCalledOnce();
    expect(confirm).toHaveBeenCalledWith(
      expect.stringContaining(
        "Pending in-memory edits will be retried after standard Markdown authority returns."
      )
    );
    expect(toasts().at(-1)?.message).toBe(
      "Private sparse-v2 recovery state was preserved."
    );
    expect(root.textContent).toContain("Enable sparse v2");
    dispose();
  });

  it("keeps the simple success path when there are no pending edits", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    const flush = vi.spyOn(store, "flushAll").mockResolvedValue(true);
    const reset = vi.spyOn(store, "resetStore");
    vi.spyOn(backend(), "cancelSparseV2").mockResolvedValue({
      status: { ...legacy(), binding_generation: 12 },
      binding_generation: 12,
      recovery_statement: "Private sparse-v2 recovery state was preserved.",
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to standard Markdown mode")
    ) as HTMLButtonElement;
    rollback.click();
    await tick();
    await tick();

    expect(flush).toHaveBeenCalledTimes(2);
    expect(reset).toHaveBeenCalledOnce();
    expect(toasts().at(-1)?.message).toBe(
      "Private sparse-v2 recovery state was preserved."
    );
    dispose();
  });

  it("keeps dirty in-memory pages when the post-rollback legacy flush still fails", async () => {
    const calls: string[] = [];
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockImplementation(async () => {
      calls.push("flush");
      return false;
    });
    const reset = vi.spyOn(store, "resetStore");
    vi.spyOn(backend(), "cancelSparseV2").mockImplementation(async () => {
      calls.push("cancel");
      const status = { ...legacy(), binding_generation: 12 };
      return {
        status,
        binding_generation: 12,
        recovery_statement: "Private sparse-v2 recovery state was preserved.",
      };
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to standard Markdown mode")
    ) as HTMLButtonElement;
    rollback.click();
    await tick();
    await tick();

    expect(calls).toEqual(["flush", "cancel", "flush"]);
    expect(reset).not.toHaveBeenCalled();
    expect(root.textContent).toContain("Enable sparse v2");
    expect(toasts().at(-1)?.message).toContain(
      "Standard Markdown mode is active, but your in-memory edits remain unsaved"
    );
    expect(toasts().at(-1)?.message).toContain("resolve conflicts or retry");
    dispose();
  });

  it("does not offer rollback when shared evidence makes it unsafe", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue({
      ...localActive(),
      can_cancel: false,
      cancel_reason: "Shared/provider sparse-v2 evidence exists.",
    });
    const cancel = vi.spyOn(backend(), "cancelSparseV2");
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    expect(root.textContent).not.toContain("Return to standard Markdown mode");
    expect(root.textContent).toContain("Shared/provider sparse-v2 evidence exists.");
    expect(cancel).not.toHaveBeenCalled();
    dispose();
  });
});

describe("Settings progressive disclosure and search", () => {
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
