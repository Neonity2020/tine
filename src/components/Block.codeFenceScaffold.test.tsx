import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { For, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { initParser } from "../render/parse";
import { doc, loadSingle, pageByName, resetStore } from "../store";
import { startEditing, endEdit } from "../editorController";
import { setAutoPairing } from "../ui";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

// GH #413: typing three backticks to open a code block is an EXPLICIT scaffold
// decision — it yields exactly three opening backticks, inserts the matching
// closing fence, and lands the caret inside (between the fences). It wins over
// the generic symmetric-backtick auto-pairing (which used to leave FOUR
// backticks and no closer). Single inline backticks and all bracket/ref
// pairing are unchanged.

beforeAll(() => initParser());

afterEach(() => {
  setAutoPairing(true);
  endEdit("page-navigation");
  resetStore();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element) {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

let seq = 0;
function mountEditor(raw: string) {
  const block: BlockDto = { id: `sf-${++seq}`, raw, collapsed: false, children: [] };
  const name = `SF${seq}`;
  const page: PageDto = { name, kind: "page", title: name, pre_block: null, blocks: [block] };
  loadSingle(page);
  startEditing(block.id, raw.length);
  const mounted = mount(() => (
    <For each={pageByName(name)?.roots ?? []}>{(id) => <Block id={id} />}</For>
  ));
  return {
    ...mounted,
    blockId: block.id,
    textarea: mounted.root.querySelector("textarea.block-editor") as HTMLTextAreaElement,
  };
}

/** Simulate one keystroke: the browser inserts the char, then onInput runs. */
function typeChar(ta: HTMLTextAreaElement, ch: string) {
  const caret = ta.selectionStart;
  ta.value = ta.value.slice(0, caret) + ch + ta.value.slice(caret);
  ta.setSelectionRange(caret + 1, caret + 1);
  ta.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: ch }));
}

describe("triple-backtick code fence scaffold (GH #413)", () => {
  it("three backticks insert a complete fence scaffold with an interior caret — exactly three, never four", async () => {
    const { textarea, blockId, dispose } = mountEditor("");
    try {
      typeChar(textarea, "`");
      // A single backtick still auto-pairs (unchanged generic behavior).
      expect(textarea.value).toBe("``");
      typeChar(textarea, "`");
      typeChar(textarea, "`");
      // The scaffold replaces the third-backtick pairing: no fourth backtick,
      // a matching closing fence appears, and the caret lands between them.
      expect(doc.byId[blockId].raw).toBe("```\n\n```");
      expect(doc.byId[blockId].raw).not.toContain("````");
      // The body-only editor now shows just the (empty) payload, caret inside.
      expect(textarea.value).not.toContain("```");
      // The view swap resolves on the microtask after the input handler.
      await vi.waitFor(() => {
        expect(textarea.selectionStart).toBe(0);
        expect(textarea.selectionEnd).toBe(0);
      });
      expect(textarea.value).not.toContain("```");
    } finally {
      dispose();
    }
  });

  it("the scaffold is a whole-block decision: three backticks mid-text keep ordinary inline behavior", () => {
    const { textarea, blockId, dispose } = mountEditor("note ");
    try {
      typeChar(textarea, "`");
      typeChar(textarea, "`");
      typeChar(textarea, "`");
      // No fence scaffold was synthesized inline: the block stays one line of prose.
      expect(doc.byId[blockId].raw).not.toContain("\n");
      expect(doc.byId[blockId].raw.startsWith("note ")).toBe(true);
    } finally {
      dispose();
    }
  });

  it("keeps pairing off behavior: with auto-pairing disabled, three backticks still scaffold", async () => {
    setAutoPairing(false);
    const { textarea, blockId, dispose } = mountEditor("");
    try {
      typeChar(textarea, "`");
      expect(textarea.value).toBe("`"); // no pairing with the toggle off
      typeChar(textarea, "`");
      typeChar(textarea, "`");
      expect(doc.byId[blockId].raw).toBe("```\n\n```");
      expect(textarea.value).not.toContain("```");
      await vi.waitFor(() => expect(textarea.selectionStart).toBe(0));
    } finally {
      dispose();
    }
  });
});
