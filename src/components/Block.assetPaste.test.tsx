import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { For, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { initParser } from "../render/parse";
import { doc, loadSingle, pageByName, resetStore } from "../store";
import { startEditing } from "../editorController";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  resetStore();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(node, root);
  return { root, dispose };
}

function blk(id: string, raw: string): BlockDto {
  return { id, raw, collapsed: false, children: [] };
}

function page(name: string, blocks: BlockDto[]): PageDto {
  return { name, kind: "page", title: name, pre_block: null, blocks };
}

function tick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

function imagePasteEvent(file: File): Event {
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", {
    value: {
      getData: () => "",
      items: [{ kind: "file", type: "image/png", getAsFile: () => file }],
      types: ["Files", "image/png"],
    },
  });
  return event;
}

describe("asset paste durability", () => {
  it("rolls back the inserted asset link if saveAsset rejects", async () => {
    loadSingle(page("Assets", [blk("asset-1", "")]));
    const id = pageByName("Assets")!.roots[0];
    startEditing(id, 0);
    vi.stubGlobal("URL", {
      ...URL,
      createObjectURL: vi.fn(() => "blob:asset"),
      revokeObjectURL: vi.fn(),
    });
    vi.spyOn(backend(), "saveAsset").mockRejectedValue(new Error("disk full"));

    const { root, dispose } = mount(() => (
      <For each={pageByName("Assets")?.roots ?? []}>{(bid) => <Block id={bid} />}</For>
    ));

    try {
      const textarea = root.querySelector("textarea") as HTMLTextAreaElement | null;
      expect(textarea).not.toBeNull();
      textarea!.focus();
      textarea!.setSelectionRange(0, 0);
      textarea!.dispatchEvent(imagePasteEvent(new File([new Uint8Array([1, 2, 3])], "paste.png", { type: "image/png" })));

      await tick();
      await tick();
      await tick();

      expect(backend().saveAsset).toHaveBeenCalledOnce();
      expect(doc.byId[id].raw).not.toContain("../assets/");
    } finally {
      dispose();
    }
  });
});
