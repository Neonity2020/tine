import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings, setToasts, toasts } from "../ui";
import { backend } from "../backend";
import { managedStorageRuntime } from "../managedStorageRuntime";
import * as store from "../store";
import type { SparseV2ActivationProgress, SparseV2Status } from "../types";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const showSparsePanel = async (root: HTMLElement) => {
  openSettings();
  await tick();
  const backupsTab = [...root.querySelectorAll(".settings-nav-item")].find(
    (button) => button.textContent === "Backups & recovery"
  ) as HTMLButtonElement;
  backupsTab.click();
  await tick();
  const experimental = [...root.querySelectorAll("button")].find(
    (button) => button.textContent?.includes("Experimental")
  ) as HTMLButtonElement;
  experimental.click();
  await tick();
};

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  localStorage.clear();
  setToasts([]);
  managedStorageRuntime.clear();
  vi.restoreAllMocks();
});

describe("Settings storage transitions", () => {
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

  it("discloses managed storage as experimental and keeps Direct files available", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    openSettings();
    await tick();
    const backupsTab = [...root.querySelectorAll(".settings-nav-item")].find(
      (button) => button.textContent === "Backups & recovery"
    ) as HTMLButtonElement;
    backupsTab.click();
    await tick();

    const experimental = [...root.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Experimental")
    ) as HTMLButtonElement;
    expect(experimental.getAttribute("aria-expanded")).toBe("false");
    expect(root.textContent).not.toContain("Enable Tine-managed storage");

    const search = root.querySelector(".settings-search-input") as HTMLInputElement;
    search.value = "storage sync";
    search.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await tick();
    const result = root.querySelector(".settings-search-result") as HTMLButtonElement;
    expect(result.textContent).toContain("Backups & recovery › Experimental");
    result.click();
    await tick();

    expect(experimental.getAttribute("aria-expanded")).toBe("true");
    expect(root.textContent).toContain("Tine-managed storage is for testing and is not yet mature.");
    expect(root.textContent).toContain("You can keep using Direct files in the meantime.");
    expect(root.textContent).toContain("Uses your graph’s Markdown or Org files directly.");
    expect(root.textContent).toContain("Enable Tine-managed storage");
    dispose();
  });

  it("flushes before setup, offers retry, and invalidates stale pages", async () => {
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
      button.textContent?.includes("Enable Tine-managed storage")
    ) as HTMLButtonElement;
    enable.click();
    await tick();
    await tick();

    expect(calls).toEqual(["flush", "activate"]);
    expect(reset).toHaveBeenCalled();
    expect(toasts().at(-1)?.message).toBe(
      "Tine-managed storage setup did not complete: Setup can be retried."
    );
    expect(root.textContent).toContain("Retry setup");
    expect(root.textContent).toContain("Setup paused. You can retry setup when you are ready.");
    expect(root.textContent).toContain("Return to Direct files");
    dispose();
  });

  it("shows the live managed-runtime reason from the shared watcher bridge", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    managedStorageRuntime.bind(11);
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    managedStorageRuntime.receiveTick({
      binding_generation: 11,
      tick: { state: "blocked", detail: "missing trusted recovery receipt", epoch: null },
    });
    managedStorageRuntime.receiveError({
      binding_generation: 11,
      message: 'Blocked("missing trusted recovery receipt")',
    });
    await tick();

    expect(root.textContent).toContain('Managed storage needs attention: Blocked("missing trusted recovery receipt")');
    dispose();
  });

  it("shows scoped indeterminate and part progress and unsubscribes for fresh setup and retry", async () => {
    const calls: string[] = [];
    const listeners: Array<(progress: SparseV2ActivationProgress) => void> = [];
    const resolvers: Array<(status: SparseV2Status) => void> = [];
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "onSparseV2ActivationProgress").mockImplementation(
      async (generation, listener) => {
        calls.push(`listen-${generation}`);
        listeners.push(listener);
        return () => calls.push(`unlisten-${generation}`);
      }
    );
    vi.spyOn(backend(), "activateSparseV2").mockImplementation(
      () => new Promise<SparseV2Status>((resolve) => {
        calls.push("activate");
        resolvers.push(resolve);
      })
    );

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const enable = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Enable Tine-managed storage")
    ) as HTMLButtonElement;
    enable.click();
    await tick();
    await tick();

    expect(calls).toEqual(["listen-10", "activate"]);
    listeners[0]({ kind: "phase", phase: "source_capture" });
    await tick();
    let progress = root.querySelector(".settings-activation-progress progress") as HTMLProgressElement;
    expect(root.textContent).toContain("Capturing source files");
    expect(progress.hasAttribute("value")).toBe(false);

    listeners[0]({ kind: "bootstrap_detached_authoring", completed: 2, total: 4 });
    await tick();
    progress = root.querySelector(".settings-activation-progress progress") as HTMLProgressElement;
    expect(root.textContent).toContain("Building operation history (2 of 4 parts)");
    expect(progress.value).toBe(2);
    expect(progress.max).toBe(4);

    resolvers[0](localRetryable());
    await tick();
    await tick();
    expect(calls).toContain("unlisten-10");
    expect(root.querySelector(".settings-activation-progress")).toBeNull();

    const retry = [...root.querySelectorAll("button")].find(
      (button) => button.textContent === "Retry setup"
    ) as HTMLButtonElement;
    retry.click();
    await tick();
    await tick();
    expect(calls.slice(-2)).toEqual(["listen-11", "activate"]);
    listeners[1]({ kind: "bootstrap_preparation_subphase", subphase: "sealing" });
    await tick();
    progress = root.querySelector(".settings-activation-progress progress") as HTMLProgressElement;
    expect(root.textContent).toContain("Sealing prepared history");
    expect(progress.hasAttribute("value")).toBe(false);
    resolvers[1](localActive());
    await tick();
    await tick();
    expect(calls).toContain("unlisten-11");
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
        recovery_statement: "Direct file mode is active. Complete recovery state was preserved.",
      };
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to Direct files")
    ) as HTMLButtonElement;
    expect(rollback).toBeTruthy();
    rollback.click();
    await tick();
    await tick();

    expect(calls).toEqual(["flush-1", "cancel", "flush-2", "reset"]);
    expect(reset).toHaveBeenCalledOnce();
    expect(confirm).toHaveBeenCalledWith(
      expect.stringContaining(
        "Pending in-memory edits will be retried after Direct files returns."
      )
    );
    expect(toasts().at(-1)?.message).toBe(
      "Direct file mode is active. Complete recovery state was preserved."
    );
    expect(root.textContent).toContain("Enable Tine-managed storage");
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
      recovery_statement: "Direct file mode is active. Complete recovery state was preserved.",
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to Direct files")
    ) as HTMLButtonElement;
    rollback.click();
    await tick();
    await tick();

    expect(flush).toHaveBeenCalledTimes(2);
    expect(reset).toHaveBeenCalledOnce();
    expect(toasts().at(-1)?.message).toBe(
      "Direct file mode is active. Complete recovery state was preserved."
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
        recovery_statement: "Direct file mode is active. Complete recovery state was preserved.",
      };
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to Direct files")
    ) as HTMLButtonElement;
    rollback.click();
    await tick();
    await tick();

    expect(calls).toEqual(["flush", "cancel", "flush"]);
    expect(reset).not.toHaveBeenCalled();
    expect(root.textContent).toContain("Enable Tine-managed storage");
    expect(toasts().at(-1)?.message).toContain(
      "Direct files is active, but your in-memory edits remain unsaved"
    );
    expect(toasts().at(-1)?.message).toContain("resolve conflicts or retry");
    dispose();
  });

  it("does not offer rollback when shared evidence makes it unsafe", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue({
      ...localActive(),
      can_cancel: false,
      cancel_reason: "Sync data already exists for another device.",
    });
    const cancel = vi.spyOn(backend(), "cancelSparseV2");
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    expect(
      [...root.querySelectorAll("button")].find(
        (button) => button.textContent === "Return to Direct files"
      )
    ).toBeUndefined();
    expect(root.textContent).toContain(
      "Return to Direct files is unavailable because safety could not be verified."
    );
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
