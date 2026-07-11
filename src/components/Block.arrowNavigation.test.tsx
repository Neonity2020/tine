import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { For, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { startEditing } from "../editorController";
import { initParser } from "../render/parse";
import { loadSingle, pageByName, resetStore } from "../store";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

function block(id: string, raw: string): BlockDto {
  return { id, raw, collapsed: false, children: [] };
}

function page(blocks: BlockDto[]): PageDto {
  return { name: "Caret", kind: "page", title: "Caret", pre_block: null, blocks };
}

function mockFiveCharacterVisualRows(): void {
  const width = 5;
  const lineHeight = 10;
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (this: HTMLElement) {
    return this instanceof HTMLDivElement ? lineHeight : 0;
  });
  vi.spyOn(HTMLElement.prototype, "offsetTop", "get").mockImplementation(function (this: HTMLElement) {
    if (!(this instanceof HTMLSpanElement)) return 0;
    const before = this.previousSibling?.textContent?.length ?? 0;
    const full = this.parentElement?.textContent?.replaceAll("\u200b", "") ?? "";
    const occupied = before === full.length && before > 0 ? before - 1 : before;
    return Math.floor(occupied / width) * lineHeight;
  });
}

describe("cross-block vertical caret navigation", () => {
  it("ArrowUp lands at the matching column on the previous block's bottom visual row", () => {
    mockFiveCharacterVisualRows();
    loadSingle(page([
      block("previous", "abcdefgh"), // visual rows: abcde / fgh
      block("current", "middle"),
    ]));
    startEditing("current", 2);
    const { root, dispose } = mount(() => (
      <For each={pageByName("Caret")?.roots ?? []}>{(id) => <Block id={id} />}</For>
    ));

    try {
      const current = root.querySelector("textarea.block-editor") as HTMLTextAreaElement;
      current.setSelectionRange(2, 2);
      current.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true, cancelable: true }));

      const previous = root.querySelector("textarea.block-editor") as HTMLTextAreaElement;
      expect(previous.value).toBe("abcdefgh");
      expect(previous.selectionStart).toBe(7); // fg|h on the bottom row, not ab|c on top
      expect(previous.selectionEnd).toBe(7);
    } finally {
      dispose();
    }
  });
});
