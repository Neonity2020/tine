import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { For, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { startEditing } from "../editorController";
import { initParser } from "../render/parse";
import { doc, loadSingle, pageByName, resetStore } from "../store";
import { formatJournal, setJournalTitleFormat } from "../journal";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  setJournalTitleFormat(null);
  resetStore();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

function inputAt(textarea: HTMLTextAreaElement, value: string, caret = value.length) {
  textarea.focus();
  textarea.value = value;
  textarea.setSelectionRange(caret, caret);
  textarea.dispatchEvent(new InputEvent("input", {
    bubbles: true,
    inputType: "insertText",
    data: value[caret - 1] ?? null,
  }));
}

function choose(label: string) {
  const item = [...document.body.querySelectorAll<HTMLElement>(".autocomplete .ac-item")]
    .find((candidate) => candidate.querySelector(".ac-label")?.textContent === label);
  expect(item).toBeDefined();
  item!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
}

function journalPage(name: string, raw: string): PageDto {
  const block: BlockDto = { id: "thatday-host", raw, collapsed: false, children: [] };
  return { name, kind: "journal", title: name, pre_block: null, format: "md", blocks: [block] };
}

function regularPage(raw: string): PageDto {
  const block: BlockDto = { id: "thatday-host", raw, collapsed: false, children: [] };
  return { name: "Notes", kind: "page", title: "Notes", pre_block: null, format: "md", blocks: [block] };
}

describe("/That day slash command (GH #252)", () => {
  it("inserts the containing journal page's date in the configured format", async () => {
    setJournalTitleFormat("yyyy-MM-dd");
    // Edit a block on the 2026-07-21 journal page
    loadSingle(journalPage("2026-07-21", "/thatday"));
    startEditing("thatday-host", 8);
    const { root, dispose } = mount(() => (
      <For each={pageByName("2026-07-21")?.roots ?? []}>{(id) => <Block id={id} />}</For>
    ));

    try {
      const textarea = root.querySelector("textarea.block-editor") as HTMLTextAreaElement;
      inputAt(textarea, "/thatday");
      await vi.waitFor(() => {
        const labels = [...document.body.querySelectorAll(".ac-label")]
          .map((element) => element.textContent);
        expect(labels).toContain("That day");
      });

      choose("That day");

      // Should insert [[2026-07-21]] — the journal page's date, NOT today
      await vi.waitFor(() => expect(doc.byId["thatday-host"].raw.trimEnd()).toBe("[[2026-07-21]]"));
    } finally {
      dispose();
    }
  });

  it("uses the page's semantic date even when the format differs from yyyy-MM-dd", async () => {
    setJournalTitleFormat("MMM do, yyyy");
    // Journal page created in yyyy-MM-dd but configured format is now "MMM do, yyyy"
    loadSingle(journalPage("2026-07-21", "/thatday"));
    startEditing("thatday-host", 8);
    const { root, dispose } = mount(() => (
      <For each={pageByName("2026-07-21")?.roots ?? []}>{(id) => <Block id={id} />}</For>
    ));

    try {
      const textarea = root.querySelector("textarea.block-editor") as HTMLTextAreaElement;
      inputAt(textarea, "/thatday");
      await vi.waitFor(() => {
        const labels = [...document.body.querySelectorAll(".ac-label")]
          .map((element) => element.textContent);
        expect(labels).toContain("That day");
      });

      choose("That day");

      const expected = `[[${formatJournal(new Date(2026, 6, 21), "MMM do, yyyy")}]]`;
      await vi.waitFor(() => expect(doc.byId["thatday-host"].raw.trimEnd()).toBe(expected));
    } finally {
      dispose();
    }
  });

  it("does nothing on a non-journal page (no insert)", async () => {
    setJournalTitleFormat("yyyy-MM-dd");
    loadSingle(regularPage("/thatday"));
    startEditing("thatday-host", 8);
    const { root, dispose } = mount(() => (
      <For each={pageByName("Notes")?.roots ?? []}>{(id) => <Block id={id} />}</For>
    ));

    try {
      const textarea = root.querySelector("textarea.block-editor") as HTMLTextAreaElement;
      inputAt(textarea, "/thatday");
      await vi.waitFor(() => {
        const labels = [...document.body.querySelectorAll(".ac-label")]
          .map((element) => element.textContent);
        expect(labels).toContain("That day");
      });

      choose("That day");

      // The slash trigger is removed but no date link is inserted
      await vi.waitFor(() => expect(doc.byId["thatday-host"].raw.trimEnd()).toBe(""));
    } finally {
      dispose();
    }
  });
});
