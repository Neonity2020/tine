import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { editingId, endEdit } from "../editorController";
import { initParser } from "../render/parse";
import { resetStore, setDoc, type FeedPage, type Node as StoreNode } from "../store";
import { Block } from "./Block";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  endEdit("page-navigation");
  resetStore();
  document.body.innerHTML = "";
});

describe("rendered block edit gesture (GH #368)", () => {
  it("mounts and focuses the editor on primary mousedown, before mouseup", () => {
    const id = "mouse-down-edit";
    const page: FeedPage = {
      name: "Mouse down",
      kind: "page",
      title: "Mouse down",
      preBlock: null,
      roots: [id],
      format: "md",
      readOnly: false,
      guide: false,
    };
    const node: StoreNode = {
      id,
      raw: "Caret now",
      collapsed: false,
      parent: null,
      page: page.name,
      children: [],
    };
    setDoc({ byId: { [id]: node }, pages: [page], feed: [], loaded: true });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <Block id={id} />, host);
    try {
      const content = host.querySelector<HTMLElement>(".block-content")!;
      const down = new MouseEvent("mousedown", {
        bubbles: true,
        cancelable: true,
        button: 0,
        clientX: 1,
        clientY: 1,
      });
      content.dispatchEvent(down);

      const editor = host.querySelector<HTMLTextAreaElement>("textarea.block-editor");
      expect(down.defaultPrevented).toBe(true);
      expect(editingId()).toBe(id);
      expect(editor).not.toBeNull();
      expect(document.activeElement).toBe(editor);
    } finally {
      dispose();
    }
  });
});
