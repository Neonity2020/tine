import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { For } from "solid-js";
import { render } from "solid-js/web";
import { initParser } from "../render/parse";
import {
  doc,
  extendSelectionTo,
  loadSingle,
  pageByName,
  resetStore,
  selectBlock,
  selectedIds,
} from "../store";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
  Reflect.deleteProperty(document, "elementFromPoint");
  document.body.innerHTML = "";
});

const block = (id: string): BlockDto => ({ id, raw: id, collapsed: false, children: [] });
const page = (): PageDto => ({
  name: "Drag",
  kind: "page",
  title: "Drag",
  pre_block: null,
  blocks: ["A", "B", "C", "D", "E"].map(block),
});

describe("Block selection drag ownership (GH #240)", () => {
  it("drags the captured selection when the pointer bullet is outside it", async () => {
    loadSingle(page());
    selectBlock("B");
    extendSelectionTo("C");

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(
      () => <For each={pageByName("Drag")?.roots ?? []}>{(id) => <Block id={id} />}</For>,
      host,
    );
    try {
      const target = host.querySelector<HTMLElement>('[data-block-id="E"]')!;
      const targetMain = target.querySelector<HTMLElement>(".block-main")!;
      vi.spyOn(targetMain, "getBoundingClientRect").mockReturnValue({
        x: 0,
        y: 100,
        top: 100,
        right: 200,
        bottom: 120,
        left: 0,
        width: 200,
        height: 20,
        toJSON: () => ({}),
      });
      Object.defineProperty(document, "elementFromPoint", {
        configurable: true,
        value: vi.fn(() => target),
      });

      const pointerBullet = host.querySelector<HTMLElement>(
        '[data-block-id="A"] > .block-main .bullet-container',
      )!;
      pointerBullet.dispatchEvent(new MouseEvent("mousedown", {
        button: 0,
        bubbles: true,
        clientX: 0,
        clientY: 0,
      }));
      document.dispatchEvent(new MouseEvent("mousemove", {
        bubbles: true,
        clientX: 10,
        clientY: 120,
      }));
      document.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
      await Promise.resolve();

      expect(pageByName("Drag")!.roots).toEqual(["A", "D", "E", "B", "C"]);
      expect(doc.byId.A.raw).toBe("A");
      expect(doc.byId.B.parent).toBeNull();
      expect(doc.byId.C.parent).toBeNull();
      expect(selectedIds()).toEqual(["B", "C"]);
    } finally {
      dispose();
    }
  });
});
