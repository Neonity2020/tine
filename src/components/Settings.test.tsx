import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, dismissToast, openSettings, setGraphMeta, setGraphTransitioning, setToasts, toasts } from "../ui";
import { backend } from "../backend";
import { managedStorageRuntime } from "../managedStorageRuntime";
import { storageTransitionRuntime } from "../storageTransitionRuntime";
import * as store from "../store";
import type { SparseV2ActivationProgress, SparseV2Status } from "../types";
import { formatJournal, parseJournalWith } from "../journal";
import {
  changeWideContentWidth,
  resetStandardContentWidth,
  standardContentWidth,
  wideContentWidth,
} from "../contentWidth";

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
  storageTransitionRuntime.clear();
  setGraphTransitioning(false);
  setGraphMeta(null);
  vi.restoreAllMocks();
  vi.useRealTimers();
  resetStandardContentWidth();
  changeWideContentWidth(null);
});

describe("Settings storage transitions", () => {
  beforeEach(() => {
    // The render harness provides a Direct Files binding for ordinary component
    // fixtures. These tests intentionally exercise backend status transitions
    // across several explicit binding generations, so they must begin unbound.
    managedStorageRuntime.clear();
    setGraphMeta({ root: "/graphs/settings-test" } as never);
  });

  const legacy = (): SparseV2Status => ({
    state: "legacy_default",
    runtime: null,
    can_activate: true,
    can_retry: false,
    can_cancel: false,
    cancel_reason: null,
    binding_generation: 10,
    application_page_admission: { binding_generation: 10, authority: "direct" },
  });
  const legacyAt = (bindingGeneration: number): SparseV2Status => ({
    ...legacy(),
    binding_generation: bindingGeneration,
    application_page_admission: {
      binding_generation: bindingGeneration,
      authority: "direct",
    },
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
      provider_runnable: false,
      search_index_building: false,
    },
    can_activate: false,
    can_retry: false,
    can_cancel: true,
    cancel_reason: null,
    binding_generation: 11,
    application_page_admission: {
      binding_generation: 11,
      authority: "managed_writable",
      application_save_page_blocks: 511,
      application_page_request_text_bytes: 1_048_576,
      application_page_max_depth: 128,
    },
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
    application_page_admission: { binding_generation: 11, authority: "managed_unavailable" },
  });

  it("discloses managed storage as known-buggy and keeps Direct files available", async () => {
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
    expect(root.textContent).toContain("Known to be buggy.");
    expect(root.textContent).toContain("does not yet fully work in our own testing; we're actively working on it.");
    // Direct files and Tine-managed storage are peers. Neither description may
    // present the other as the destination, and the panel says so where a user
    // deciding between them will read it.
    expect(root.textContent).toContain(
      "Direct files is a permanent, fully supported way to use Tine — not a step on the way to anything."
    );
    expect(root.textContent).toContain("Many people will want to stay here.");
    expect(root.textContent).toContain("Enable Tine-managed storage");
    expect(root.textContent).toContain("Join a synced graph from another device");
    dispose();
  });

  it("starts shared discovery only after an explicit Direct Files join action", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    const join = vi.spyOn(backend(), "joinSparseV2Shared").mockRejectedValue(
      new Error("managed join could not find a shared provider descriptor")
    );

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    expect(join).not.toHaveBeenCalled();
    const button = [...root.querySelectorAll("button")].find((candidate) =>
      candidate.textContent?.includes("Join a synced graph from another device")
    ) as HTMLButtonElement;
    button.click();
    await tick();
    await tick();

    expect(join).toHaveBeenCalledTimes(1);
    expect(toasts().at(-1)).toMatchObject({
      message: "Couldn't join the synced graph: managed join could not find a shared provider descriptor",
      sticky: true,
      action: { label: "Copy details" },
    });
    dispose();
  });

  it("names an affected note when a clean sync join finds semantic divergence", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "joinSparseV2Shared").mockRejectedValue(
      new Error(
        "managed sync join failed at provider scan: sync actor refused request: sync join refused: notes not in the shared provider frontier; local-pages=1059 shared-pages=1059 local-only=0 shared-only=0 changed=1 (kind=0 preamble=0 outline=1 explicit-ids=0); authorities unchanged\n"
        + 'clean join mismatch detail: changed path="journals/2026_08_25.md" categories=outline',
      ),
    );

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    const button = [...root.querySelectorAll("button")].find((candidate) =>
      candidate.textContent?.includes("Join a synced graph from another device")
    ) as HTMLButtonElement;
    button.click();
    await tick();
    await tick();

    const failure = toasts().at(-1);
    expect(failure).toMatchObject({ sticky: true, action: { label: "Copy details" } });
    expect(failure?.message).toContain('Changed (blocks, content, or order): "journals/2026_08_25.md"');
    expect(failure?.message).toContain("Nothing was changed on either device");
    dispose();
  });

  it("keeps the not-yet refusal actionable after the panel truncates it to one line", async () => {
    // The native message names the file and both ordinary causes; the panel
    // shows only its first line, which is the dead end. The remedy carries the
    // rest, or a real device is told nothing it can act on.
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "joinSparseV2Shared").mockRejectedValue(
      new Error(
        "This graph does not yet contain sync data from another device.\n\n"
        + "Tine looked for /graphs/notes/.tine-sync/v2/shared/outbox/enrollment/shared-enrollment-v1.json.\n\n"
        + "Two things usually explain that."
      )
    );

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    const button = [...root.querySelectorAll("button")].find((candidate) =>
      candidate.textContent?.includes("Join a synced graph from another device")
    ) as HTMLButtonElement;
    button.click();
    await tick();
    await tick();

    const message = String(toasts().at(-1)?.message ?? "");
    expect(message).toContain("does not yet contain sync data from another device");
    expect(message).toContain(
      ".tine-sync/v2/shared/outbox/enrollment/shared-enrollment-v1.json"
    );
    expect(message).toContain("Set up sync with another device");
    expect(message).toContain("skip dot-directories");
    expect(message).toContain("Nothing was changed on this device.");
    dispose();
  });

  const sharedActive = (): SparseV2Status => {
    const base = localActive();
    return {
      ...base,
      runtime: { ...base.runtime!, shared_role: "initiator", shared_phase: "active" },
    };
  };

  it("offers the join action from Tine-managed storage and names what happens to this device's own history", async () => {
    // The native join branch accepts a device that already holds managed
    // storage (`prepare_sparse_v2_join`'s `slot.sparse_binding().is_some()`
    // path), so hiding the action behind Direct files made a supported action
    // invisible — the worst of the three options.
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(false);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    const join = vi.spyOn(backend(), "joinSparseV2Shared");

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    const button = [...root.querySelectorAll("button")].find((candidate) =>
      candidate.textContent?.includes("Join a synced graph from another device")
    ) as HTMLButtonElement;
    expect(button).toBeTruthy();
    button.click();
    await tick();
    await tick();

    const prompt = confirm.mock.calls[0][0];
    // The exact consequence, not "this may replace data": the shared baseline
    // and operation archive REPLACE this device's own, the replaced pair is
    // deleted, and the swap happens only when the notes already match.
    expect(prompt).toContain("its own operation history and baseline are replaced by the shared ones");
    expect(prompt).toContain("the replaced pair is deleted");
    expect(prompt).toContain("already identical on both sides");
    // And the other branch: a different history changes nothing at all.
    expect(prompt).toContain("Tine changes nothing at all");
    // And that the dead end has an exit: a second prompt, which is where the
    // archive location is named.
    expect(prompt).toContain("offers to ADOPT the other device's graph instead");
    expect(prompt).toContain("nothing happens until you accept that second prompt");
    // Declining leaves the graph exactly as it was.
    expect(join).not.toHaveBeenCalled();
    expect(toasts()).toEqual([]);
    dispose();
  });

  const independentHistoryRefusal = () =>
    new Error(
      "managed sync join failed at provider scan: sync actor refused request: "
        + "clean shared descriptor names another managed graph"
    );

  const clickManagedJoin = async (root: HTMLElement) => {
    const button = [...root.querySelectorAll("button")].find((candidate) =>
      candidate.textContent?.includes("Join a synced graph from another device")
    ) as HTMLButtonElement;
    button.click();
    await tick();
    await tick();
    await tick();
  };

  it("offers adoption when the shared graph names another managed history, and says where this device's own goes", async () => {
    // Two devices that each enabled managed storage on their own can never
    // join each other: the native branch compares workspace identity first.
    // The refusal is correct, and adoption is the operation behind it.
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "joinSparseV2Shared").mockRejectedValue(independentHistoryRefusal());
    vi.spyOn(backend(), "sparseV2RecoveryLocation").mockResolvedValue(
      "/home/example/.local/share/tine/managed-history-archive"
    );
    const adopt = vi.spyOn(backend(), "adoptSparseV2Shared").mockResolvedValue({
      status: {
        ...localActive(),
        binding_generation: 12,
        runtime: { ...localActive().runtime!, shared_role: "joiner", shared_phase: "active" },
        application_page_admission: {
          ...localActive().application_page_admission,
          binding_generation: 12,
        },
      },
      binding_generation: 12,
      archive_location: "/home/example/.local/share/tine/managed-history-archive/graph-7",
      adoption_statement:
        "This device now serves the graph shared by your other device. Its own previous Tine-managed "
        + "history was archived at /home/example/.local/share/tine/managed-history-archive/graph-7 and was not merged.",
    });
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(true);

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    await clickManagedJoin(root);

    expect(adopt).toHaveBeenCalledTimes(1);
    const prompt = confirm.mock.calls[1][0];
    // The archive location is stated BEFORE the operation, not only in the
    // receipt: an archive nobody can find is not a backup.
    expect(prompt).toContain("/home/example/.local/share/tine/managed-history-archive");
    expect(prompt).toContain("archived whole, not deleted");
    // The divergence, named. This is not a merge.
    expect(prompt).toContain("Tine will not merge two histories");
    expect(prompt).toContain("Nothing from this device's own managed history is carried across");
    expect(prompt).toContain("they must already match the shared graph's files");
    expect(prompt).toContain("Cancel now and this device is left exactly as it is");
    expect(toasts().at(-1)?.message).toContain("was archived at");
    dispose();
  });

  it("keeps the refusal and its remedy when the adoption prompt is declined", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "joinSparseV2Shared").mockRejectedValue(independentHistoryRefusal());
    vi.spyOn(backend(), "sparseV2RecoveryLocation").mockResolvedValue("/archive/root");
    const adopt = vi.spyOn(backend(), "adoptSparseV2Shared");
    let call = 0;
    vi.spyOn(backend(), "confirm").mockImplementation(async () => {
      call += 1;
      return call === 1;
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    await clickManagedJoin(root);

    expect(adopt).not.toHaveBeenCalled();
    const message = toasts().at(-1)?.message ?? "";
    expect(message).toContain("Nothing was changed on either device");
    expect(message).toContain("Tine will not merge two histories");
    expect(message).toContain("archives this device's own history rather than deleting it");
    dispose();
  });

  it("reports a failed adoption without claiming the shared graph was joined", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "joinSparseV2Shared").mockRejectedValue(independentHistoryRefusal());
    vi.spyOn(backend(), "sparseV2RecoveryLocation").mockResolvedValue("/archive/root");
    vi.spyOn(backend(), "adoptSparseV2Shared").mockRejectedValue(
      new Error(
        "This device's Tine-managed storage is already shared with, or joined to, another device. Nothing was changed."
      )
    );
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    await clickManagedJoin(root);

    const message = toasts().at(-1)?.message ?? "";
    expect(message).toContain("Couldn't adopt the shared graph");
    expect(message).toContain("already shared with, or joined to, another device");
    expect(message).toContain("Nothing was changed");
    expect(message).not.toContain("joined the synced graph");
    dispose();
  });

  it("leaves the graph exactly as it is when the share confirmation is declined", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(false);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    const share = vi.spyOn(backend(), "prepareSparseV2Share");

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const button = [...root.querySelectorAll("button")].find((candidate) =>
      candidate.textContent?.includes("Set up sync with another device")
    ) as HTMLButtonElement;
    button.click();
    await tick();
    await tick();

    // The confirmation is the last moment at which nothing has been written,
    // and it says so rather than letting the user find out afterwards.
    expect(confirm.mock.calls[0][0]).toContain("Cancel now and this graph is left exactly as it is");
    expect(confirm.mock.calls[0][0]).toContain("cannot be un-shared");
    expect(share).not.toHaveBeenCalled();
    expect(toasts()).toEqual([]);
    expect(root.textContent).toContain("Set up sync with another device");
    dispose();
  });

  it("says what a shared graph's state is and where its only exit is", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(sharedActive());

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    expect(root.textContent).toContain("This graph is shared.");
    expect(root.textContent).toContain("Join a synced graph from another device");
    expect(root.textContent).toContain("Sharing cannot be switched off again");
    expect(root.textContent).toContain("Return to Direct files");
    dispose();
  });

  it("flushes before setup and leaves the serving Direct renderer intact on candidate failure", async () => {
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
    expect(reset).not.toHaveBeenCalled();
    expect(toasts().at(-1)).toMatchObject({
      message: "Tine-managed storage setup did not complete: projection proof paused on the exact test cut",
      kind: "error",
      sticky: true,
    });
    expect(root.textContent).toContain("Retry setup");
    expect(root.textContent).toContain("Setup paused. You can retry setup when you are ready.");
    expect(root.textContent).toContain("Return to Direct files");
    dispose();
  });

  it("accepts the native readiness receipt without a second frontend page probe", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "listPages");
    const loadPage = vi.spyOn(backend(), "getPageByPath");
    vi.spyOn(backend(), "activateSparseV2").mockResolvedValue(localActive());

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    loadPage.mockClear();
    const enable = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Enable Tine-managed storage")
    ) as HTMLButtonElement;
    enable.click();
    await vi.waitFor(() => expect(toasts().at(-1)).toMatchObject({
      message: "Tine-managed storage is active.",
      kind: "success",
    }));
    // Renderer rebinding may refresh ordinary page-derived resources, but it
    // must not run the former representative-page readiness proof.
    expect(loadPage).not.toHaveBeenCalled();
    dispose();
  });

  it("keeps setup failure detail visible until dismissed and lets the user copy it", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "activateSparseV2").mockResolvedValue(localRetryable());
    const writeText = vi.spyOn(backend(), "writeText").mockResolvedValue(undefined);

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

    const failure = toasts().at(-1)!;
    expect(failure.sticky).toBe(true);
    vi.useFakeTimers();
    await vi.advanceTimersByTimeAsync(3201);
    expect(toasts().some((toast) => toast.id === failure.id)).toBe(true);

    const copy = [...root.querySelectorAll("button")].find(
      (button) => button.textContent === "Copy details"
    ) as HTMLButtonElement;
    expect(root.textContent).toContain("Setup: projection proof paused on the exact test cut");
    copy.click();
    await vi.runAllTimersAsync();
    expect(writeText).toHaveBeenCalledWith("Setup: projection proof paused on the exact test cut");

    dismissToast(failure.id);
    expect(toasts().some((toast) => toast.id === failure.id)).toBe(false);
    dispose();
  });

  it("keeps backend activation errors instead of replacing them with a generic retry message", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "activateSparseV2").mockRejectedValue(new Error("native activation rejected the prepared recovery state"));

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

    expect(toasts().at(-1)).toMatchObject({
      message: "Tine-managed storage was not enabled: native activation rejected the prepared recovery state",
      sticky: true,
    });
    dispose();
  });

  it("redacts paths and bounds unstructured managed-storage errors", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(legacy());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "activateSparseV2").mockRejectedValue(new Error(
      "materialization failed at /home/martin/private/Tine.md, file:///home/martin/private/Graph.md, C:\\Users\\Martin\\Graph\\Tine.md, and \\\\server\\share\\Graph\\Tine.md; " + "diagnostic ".repeat(80)
    ));

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

    const detail = toasts().at(-1)?.message ?? "";
    expect(detail).toContain("materialization failed at [path]");
    expect(detail.match(/\[path\]/g)?.length).toBe(4);
    expect(detail).not.toContain("/home/martin");
    expect(detail).not.toContain("C:\\Users");
    expect(detail).not.toContain("server\\share");
    expect(detail.length).toBeLessThanOrEqual("Tine-managed storage was not enabled: ".length + 280);
    expect(detail).toMatch(/…$/u);
    dispose();
  });

  it("keeps share setup failures sticky with their backend detail", async () => {
    const retry = { ...localRetryable(), binding_generation: 11 };
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "prepareSparseV2Share").mockResolvedValue(retry);

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    const share = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Set up sync with another device")
    ) as HTMLButtonElement;
    share.click();
    await tick();
    await tick();
    expect(toasts().at(-1)).toMatchObject({
      message: "Sync setup did not complete: projection proof paused on the exact test cut",
      sticky: true,
    });
    dispose();
  });

  it("keeps join setup failures sticky with their backend detail", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue({
      state: "joinable",
      descriptor_digest: "test-descriptor",
      runtime: null,
      can_activate: false,
      can_retry: false,
      can_cancel: false,
      cancel_reason: null,
      binding_generation: 15,
      application_page_admission: { binding_generation: 15, authority: "direct" },
    });
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "joinSparseV2Shared").mockResolvedValue({
      ...localRetryable(),
      binding_generation: 15,
      application_page_admission: {
        binding_generation: 15,
        authority: "managed_unavailable",
      },
    });

    const joinRoot = document.createElement("div");
    document.body.append(joinRoot);
    const joinDispose = render(() => <Settings />, joinRoot);
    await showSparsePanel(joinRoot);
    const join = [...joinRoot.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Join this synced graph")
    ) as HTMLButtonElement;
    join.click();
    await tick();
    await tick();
    expect(toasts().at(-1)).toMatchObject({
      message: "Joining the synced graph did not complete: projection proof paused on the exact test cut",
      sticky: true,
    });
    joinDispose();
  });

  it("keeps the refused causal class while redacting paths from all structured diagnostics", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue({
      state: "refused",
      reason_code: "local_active",
      scenario_id: "MS-REF-DISK-CORRUPT",
      detail: "SQLite materialization failed: immutable archive error: document scratch index failed: malformed scratch blob at /home/martin/private/.tine-sync/v2/blobs.data",
      runtime: null,
      can_activate: false,
      can_retry: true,
      can_cancel: false,
      cancel_reason: "The managed recovery archive at C:\\Users\\Martin\\Graph\\.tine-sync could not be verified.",
      binding_generation: 12,
      application_page_admission: { binding_generation: 12, authority: "managed_unavailable" },
    });
    const writeText = vi.spyOn(backend(), "writeText").mockResolvedValue(undefined);
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    managedStorageRuntime.receiveError({
      binding_generation: 12,
      message: 'LeaseContended("/var/lib/tine/private/lease")',
    });
    await tick();

    expect(root.textContent).toContain("document scratch index failed: malformed scratch blob");
    expect(root.textContent).toContain("MS-REF-DISK-CORRUPT");
    expect(root.textContent).toContain("LeaseContended(\"[path]\")");
    expect(root.textContent).not.toContain("/home/martin");
    expect(root.textContent).not.toContain("C:\\Users\\Martin");
    expect(root.textContent).not.toContain("/var/lib/tine");
    const copy = [...root.querySelectorAll("button")].find(
      (button) => button.textContent === "Copy details"
    ) as HTMLButtonElement;
    copy.click();
    await tick();
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining("malformed scratch blob"));
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining("MS-REF-DISK-CORRUPT"));
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining("Return to Direct files:"));
    const copied = writeText.mock.calls.at(-1)?.[0] ?? "";
    expect(copied).toContain("[path]");
    expect(copied).not.toContain("/home/martin");
    expect(copied).not.toContain("C:\\Users\\Martin");
    expect(copied).not.toContain("/var/lib/tine");
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

    expect(root.textContent).toContain('Managed storage needs attention: Blocked("[redacted]")');
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
    let activationOperation = 100;
    vi.spyOn(backend(), "activateSparseV2").mockImplementation(
      () => new Promise<SparseV2Status>((resolve) => {
        calls.push("activate");
        storageTransitionRuntime.receive({
          operationId: activationOperation,
          window: "main",
          kind: "activate_managed",
          phase: "activating_managed",
          elapsedMs: 0,
          terminal: false,
        });
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

    listeners[0]({ kind: "phase", phase: "sqlite_open_build" });
    await tick();
    progress = root.querySelector(".settings-activation-progress progress") as HTMLProgressElement;
    expect(root.textContent).toContain("Building the local index");
    expect(progress.hasAttribute("value")).toBe(false);

    listeners[0]({ kind: "phase", phase: "retained_runtime_open" });
    await tick();
    expect(root.textContent).toContain("Opening retained managed state");

    storageTransitionRuntime.receive({
      operationId: activationOperation,
      window: "main",
      kind: "activate_managed",
      phase: "activating_managed",
      elapsedMs: 12,
      terminal: true,
      outcome: "failed",
    });
    resolvers[0](localRetryable());
    await tick();
    await tick();
    expect(calls).toContain("unlisten-10");
    expect(root.querySelector(".settings-activation-progress")).toBeNull();

    const retry = [...root.querySelectorAll("button")].find(
      (button) => button.textContent === "Retry setup"
    ) as HTMLButtonElement;
    retry.click();
    activationOperation += 1;
    await tick();
    await tick();
    expect(calls.slice(-2)).toEqual(["listen-11", "activate"]);
    listeners[1]({ kind: "phase", phase: "immutable_publication_install" });
    await tick();
    progress = root.querySelector(".settings-activation-progress progress") as HTMLProgressElement;
    expect(root.textContent).toContain("Installing prepared history");
    expect(progress.hasAttribute("value")).toBe(false);
    storageTransitionRuntime.receive({
      operationId: activationOperation,
      window: "main",
      kind: "activate_managed",
      phase: "activating_managed",
      elapsedMs: 9,
      terminal: true,
      outcome: "succeeded",
    });
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
      const status = legacyAt(12);
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
        "continuing may omit in-memory managed edits that are not yet durable"
      )
    );
    expect(confirm).toHaveBeenCalledWith(
      expect.stringContaining("archive the complete durable managed-storage and provider state")
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
      status: legacyAt(12),
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

  it("uses the independent cold escape when the managed actor cannot shut down", async () => {
    const calls: string[] = [];
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    const flush = vi.spyOn(store, "flushAll").mockResolvedValue(true);
    const reset = vi.spyOn(store, "resetStore");
    vi.spyOn(backend(), "cancelSparseV2").mockRejectedValue(
      new Error("sync actor is unavailable")
    );
    vi.spyOn(backend(), "cancelSparseV2Cold").mockImplementation(async (path) => {
      calls.push(`cold-${path}`);
      return {
        status: legacyAt(12),
        binding_generation: 12,
        recovery_statement:
          "Direct Files is active from the current Markdown/Org tree. Managed-storage evidence was left untouched.",
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

    expect(confirm).toHaveBeenCalledTimes(2);
    expect(confirm.mock.calls[1][0]).toContain("emergency exit");
    expect(confirm.mock.calls[1][0]).toContain("sync actor is unavailable");
    expect(calls).toEqual(["cold-/graphs/settings-test"]);
    expect(flush).toHaveBeenCalledOnce();
    expect(reset).toHaveBeenCalledOnce();
    expect(toasts().at(-1)?.message).toContain("Direct Files is active");
    dispose();
  });

  it("bounds cooperative shutdown before offering the independent cold escape", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue(localActive());
    vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    vi.spyOn(store, "flushAll").mockResolvedValue(true);
    vi.spyOn(backend(), "cancelSparseV2").mockImplementation(
      () => new Promise(() => {})
    );
    const cold = vi.spyOn(backend(), "cancelSparseV2Cold").mockResolvedValue({
      status: legacyAt(12),
      binding_generation: 12,
      recovery_statement: "Direct Files is active.",
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    vi.useFakeTimers();
    const rollback = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Return to Direct files")
    ) as HTMLButtonElement;
    rollback.click();
    await vi.advanceTimersByTimeAsync(10_000);
    await Promise.resolve();
    await Promise.resolve();

    expect(cold).toHaveBeenCalledWith("/graphs/settings-test");
    expect(toasts().at(-1)?.message).toBe("Direct Files is active.");
    dispose();
  });

  it("keeps the independent Direct Files escape available while managed status is unavailable", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockRejectedValue(
      new Error("managed actor did not answer")
    );
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    const cold = vi.spyOn(backend(), "cancelSparseV2Cold").mockResolvedValue({
      status: legacyAt(13),
      binding_generation: 13,
      recovery_statement: "Direct Files is active from the current Markdown/Org tree.",
    });

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);
    await tick();
    const escape = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Open current files in Direct Files")
    ) as HTMLButtonElement;
    expect(escape).toBeTruthy();
    escape.click();
    await tick();
    await tick();

    expect(confirm).toHaveBeenCalledOnce();
    expect(confirm.mock.calls[0][0]).toContain("managed storage status is unavailable");
    expect(cold).toHaveBeenCalledWith("/graphs/settings-test");
    expect(toasts().at(-1)?.message).toContain("Direct Files is active");
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
      const status = legacyAt(12);
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

  it("offers rollback with an explicit warning when shared evidence is present", async () => {
    vi.spyOn(backend(), "sparseV2Status").mockResolvedValue({
      ...localActive(),
      can_cancel: false,
      cancel_reason: "Sync data already exists for another device.",
    });
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(false);
    const cancel = vi.spyOn(backend(), "cancelSparseV2");
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <Settings />, root);
    await showSparsePanel(root);

    expect(
      [...root.querySelectorAll("button")].find(
        (button) => button.textContent === "Return to Direct files"
      )
    ).toBeTruthy();
    expect(root.textContent).toContain(
      "Sync data already exists for another device."
    );
    expect(root.textContent).toContain("might not include every durable managed or sync change");
    const rollback = [...root.querySelectorAll("button")].find(
      (button) => button.textContent === "Return to Direct files"
    ) as HTMLButtonElement;
    rollback.click();
    await tick();
    expect(confirm).toHaveBeenCalledWith(expect.stringContaining("recovery exit, not confirmation"));
    expect(cancel).not.toHaveBeenCalled();
    dispose();
  });
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
