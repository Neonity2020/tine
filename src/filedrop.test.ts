import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { insertDroppedFiles } from "./filedrop";
import { managedStorageRuntime } from "./managedStorageRuntime";
import { __setStoreMutationObserverForTest, doc, loadSingle, pageByName, resetStore } from "./store";
import { setToasts, toasts } from "./ui";
import { initParser } from "./render/parse";

const TARGET = "99999999-9999-4999-8999-999999999999";

beforeAll(() => initParser());

function seedNearFullPage(): void {
  loadSingle({
    name: "Drop",
    kind: "page",
    title: "Drop",
    pre_block: null,
    blocks: [
      ...Array.from({ length: 510 }, (_, index) => ({
        id: `00000000-0000-4000-8000-${(index + 1).toString(16).padStart(12, "0")}`,
        raw: `existing ${index}`,
        collapsed: false,
        children: [],
      })),
      { id: TARGET, raw: "target", collapsed: false, children: [] },
    ],
  });
}

function managedWritable(): void {
  managedStorageRuntime.bind(1);
  managedStorageRuntime.receiveStatus({
    state: "active",
    runtime: null,
    can_activate: false,
    can_retry: false,
    can_cancel: false,
    cancel_reason: null,
    binding_generation: 1,
    application_page_admission: {
      binding_generation: 1,
      authority: "managed_writable",
      application_save_page_blocks: 511,
      application_page_request_text_bytes: 1_048_576,
      application_page_max_depth: 128,
    },
  } as any);
}

afterEach(() => {
  __setStoreMutationObserverForTest(null);
  managedStorageRuntime.clear();
  setToasts([]);
  resetStore();
  vi.restoreAllMocks();
});

describe("managed file-drop admission", () => {
  it("refuses a null route record before a mixed asset and CSV can begin I/O or mutate", async () => {
    loadSingle({
      name: "Drop",
      kind: "page",
      title: "Drop",
      pre_block: null,
      blocks: [{ id: TARGET, raw: "target", collapsed: false, children: [] }],
    });
    const readTextFile = vi.spyOn(backend(), "readTextFile");
    const importAsset = vi.spyOn(backend(), "importAsset");
    const counts = { publications: 0, dirty: 0, undo: 0 };
    __setStoreMutationObserverForTest((event) => {
      if (event.kind === "publication") counts.publications++;
      else if (event.kind === "dirty") counts.dirty++;
      else if (event.kind === "undo-snapshot") counts.undo++;
    });

    await insertDroppedFiles(TARGET, ["/tmp/image.png", "/tmp/huge.csv"]);

    expect(readTextFile).not.toHaveBeenCalled();
    expect(importAsset).not.toHaveBeenCalled();
    expect(pageByName("Drop")!.roots).toEqual([TARGET]);
    expect(counts).toEqual({ publications: 0, dirty: 0, undo: 0 });
    expect(toasts().map(({ message }) => message)).toEqual([
      "Can't insert while Tine-managed storage is changing state. Nothing was changed.",
    ]);
  });

  it("plans a mixed 5,000-cell CSV drop before any ordinary asset import", async () => {
    seedNearFullPage();
    managedWritable();
    vi.spyOn(backend(), "readTextFile").mockResolvedValue(
      Array.from({ length: 5_000 }, (_, index) => `row ${index}`).join("\n"),
    );
    const importAsset = vi.spyOn(backend(), "importAsset").mockResolvedValue("image.png");
    const counts = { publications: 0, dirty: 0, undo: 0 };
    __setStoreMutationObserverForTest((event) => {
      if (event.kind === "publication") counts.publications++;
      else if (event.kind === "dirty") counts.dirty++;
      else if (event.kind === "undo-snapshot") counts.undo++;
    });

    await insertDroppedFiles(TARGET, ["/tmp/image.png", "/tmp/huge.csv"]);

    expect(backend().readTextFile).toHaveBeenCalledWith("/tmp/huge.csv");
    expect(importAsset).not.toHaveBeenCalled();
    expect(pageByName("Drop")!.roots).toHaveLength(511);
    expect(doc.byId[TARGET].raw).toBe("target");
    expect(counts).toEqual({ publications: 0, dirty: 0, undo: 0 });
    expect(toasts().map(({ message }) => message)).toEqual([
      "Can't insert: this page would exceed Tine-managed storage's 511-block or request-size limit. Nothing was changed.",
    ]);
  });

  it("keeps Direct Files sequential file ordering and does not force a whole-drop plan", async () => {
    loadSingle({
      name: "Drop",
      kind: "page",
      title: "Drop",
      pre_block: null,
      blocks: [{ id: TARGET, raw: "target", collapsed: false, children: [] }],
    });
    managedStorageRuntime.bind(1, { binding_generation: 1, authority: "direct" });
    const order: string[] = [];
    vi.spyOn(backend(), "importAsset").mockImplementation(async () => {
      order.push("asset");
      return "image.png";
    });
    vi.spyOn(backend(), "readTextFile").mockImplementation(async () => {
      order.push("csv");
      return "one\ntwo";
    });

    await insertDroppedFiles(TARGET, ["/tmp/image.png", "/tmp/small.csv"]);

    expect(order).toEqual(["asset", "csv"]);
    expect(toasts().map(({ message }) => message)).toEqual(["Inserted 2 files"]);
    expect(pageByName("Drop")!.roots).toHaveLength(3);
  });

  it("refuses a Direct drop that resumes after the graph became managed", async () => {
    loadSingle({
      name: "Drop",
      kind: "page",
      title: "Drop",
      pre_block: null,
      blocks: [{ id: TARGET, raw: "target", collapsed: false, children: [] }],
    });
    managedStorageRuntime.bind(1, { binding_generation: 1, authority: "direct" });
    // The user finishes enabling managed storage while the dropped file is still
    // being read. The Direct continuation must not resume past that (GH #325).
    vi.spyOn(backend(), "readTextFile").mockImplementation(async () => {
      managedWritable();
      return "one\ntwo";
    });
    const importAsset = vi.spyOn(backend(), "importAsset");

    await insertDroppedFiles(TARGET, ["/tmp/small.csv", "/tmp/image.png"]);

    expect(importAsset).not.toHaveBeenCalled();
    expect(pageByName("Drop")!.roots).toEqual([TARGET]);
    expect(doc.byId[TARGET].raw).toBe("target");
    expect(toasts().map(({ message }) => message)).toEqual([
      "Couldn't insert the dropped files: this graph changed while they were being read.",
    ]);
  });

  it("admits a small mixed drop after managed planning", async () => {
    loadSingle({
      name: "Drop",
      kind: "page",
      title: "Drop",
      pre_block: null,
      blocks: [{ id: TARGET, raw: "target", collapsed: false, children: [] }],
    });
    managedWritable();
    const order: string[] = [];
    vi.spyOn(backend(), "readTextFile").mockImplementation(async () => {
      order.push("csv-plan");
      return "one\ntwo";
    });
    vi.spyOn(backend(), "importAsset").mockImplementation(async () => {
      order.push("asset-import");
      return "image.png";
    });

    await insertDroppedFiles(TARGET, ["/tmp/image.png", "/tmp/small.csv"]);

    expect(order).toEqual(["csv-plan", "asset-import"]);
    expect(toasts().map(({ message }) => message)).toEqual(["Inserted 2 files"]);
    expect(pageByName("Drop")!.roots).toHaveLength(3);
  });
});
