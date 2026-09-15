import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { insertDroppedFiles } from "./filedrop";
import { graphBindingRuntime } from "./graphBindingRuntime";
import { __setStoreMutationObserverForTest, loadSingle, pageByName, resetStore } from "./store";
import { setToasts, toasts } from "./ui";
import { initParser } from "./render/parse";

const TARGET = "99999999-9999-4999-8999-999999999999";

beforeAll(() => initParser());

afterEach(() => {
  __setStoreMutationObserverForTest(null);
  graphBindingRuntime.clear();
  setToasts([]);
  resetStore();
  vi.restoreAllMocks();
});

describe("file-drop admission", () => {
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
      "Can't insert while the graph is changing. Nothing was changed.",
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
    graphBindingRuntime.bind(1, { binding_generation: 1 });
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
});
