import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { For } from "solid-js";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { startEditing } from "../editorController";
import { initParser } from "../render/parse";
import { __setStoreMutationObserverForTest, doc, loadSingle, pageByName, resetStore } from "../store";
import type { GraphMeta, PageDto } from "../types";
import { bumpDataRev, setGraphMeta, setToasts, toasts } from "../ui";
import { managedStorageRuntime } from "../managedStorageRuntime";
import { Block } from "./Block";

const META: GraphMeta = {
  root: "/tmp/template-graph", journals_dir: "journals", pages_dir: "pages", preferred_workflow: "now",
  shortcuts: {}, start_of_week: 6, block_hidden_properties: [], linked_references_collapsed_threshold: 100, default_journal_template: null,
  favorites: [], journal_page_title_format: "MMM do, yyyy", journal_file_name_format: "yyyy_MM_dd",
  preferred_format: "md", macros: {}, enable_timetracking: true, show_brackets: true, logbook_with_second_support: true,
  logbook_enabled_in_timestamped_blocks: false, logbook_enabled_in_all_blocks: false, guide_announced: true,
};

beforeAll(() => initParser());

afterEach(() => {
  __setStoreMutationObserverForTest(null);
  managedStorageRuntime.clear();
  vi.restoreAllMocks();
  setGraphMeta(null);
  setToasts([]);
  resetStore();
  document.body.innerHTML = "";
});

it("routes slash-template insertion through applyTemplateVars with the current page", async () => {
  vi.spyOn(backend(), "listTemplates").mockResolvedValue([{
    name: "Daily",
    page: "Templates",
    kind: "page",
    blocks: [{ id: "template", raw: "on <% current page %>", collapsed: false, children: [] }],
  }]);
  setGraphMeta(META);
  const page: PageDto = {
    name: "Shared", kind: "page", title: "Shared", pre_block: null,
    blocks: [{ id: "host", raw: "/Daily", collapsed: false, children: [] }],
  };
  loadSingle(page);
  startEditing("host", "/Daily".length);

  const root = document.createElement("div");
  document.body.append(root);
  const dispose = render(() => (
    <For each={pageByName("Shared")?.roots ?? []}>{(id) => <Block id={id} />}</For>
  ), root);
  try {
    const textarea = root.querySelector<HTMLTextAreaElement>("textarea.block-editor")!;
    textarea.focus();
    textarea.dispatchEvent(new InputEvent("input", {
      bubbles: true, inputType: "insertText", data: "y",
    }));
    await vi.waitFor(() => expect(document.body.querySelector(".autocomplete .ac-label")?.textContent).toBe("Template: Daily"));
    textarea.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Enter", code: "Enter", bubbles: true, cancelable: true,
    }));
    await vi.waitFor(() => expect(Object.values(doc.byId).map((block) => block.raw)).toContain("on [[Shared]]"));
  } finally {
    dispose();
  }
});

it("refuses an overflowing template before it removes the slash trigger", async () => {
  vi.spyOn(backend(), "listTemplates").mockResolvedValue([{
    name: "Daily",
    page: "Templates",
    kind: "page",
    blocks: [
      { id: "template-1", raw: "first", collapsed: false, children: [] },
      { id: "template-2", raw: "second", collapsed: false, children: [] },
    ],
  }]);
  setGraphMeta(META);
  const host = "99999999-9999-4999-8999-999999999999";
  const page: PageDto = {
    name: "Shared",
    kind: "page",
    title: "Shared",
    pre_block: null,
    blocks: [
      ...Array.from({ length: 510 }, (_, index) => ({
        id: `00000000-0000-4000-8000-${(index + 1).toString(16).padStart(12, "0")}`,
        raw: `existing ${index}`,
        collapsed: false,
        children: [],
      })),
      { id: host, raw: "/Daily", collapsed: false, children: [] },
    ],
  };
  loadSingle(page);
  bumpDataRev();
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
  startEditing(host, "/Daily".length);
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = render(() => (
    <For each={pageByName("Shared")?.roots ?? []}>{(id) => <Block id={id} />}</For>
  ), root);
  const counts = { publications: 0, dirty: 0, undo: 0 };
  __setStoreMutationObserverForTest((event) => {
    if (event.kind === "publication") counts.publications++;
    else if (event.kind === "dirty") counts.dirty++;
    else if (event.kind === "undo-snapshot") counts.undo++;
  });
  try {
    const textarea = root.querySelector<HTMLTextAreaElement>("textarea.block-editor")!;
    textarea.focus();
    textarea.dispatchEvent(new InputEvent("input", {
      bubbles: true, inputType: "insertText", data: "y",
    }));
    await vi.waitFor(() => expect(document.body.querySelector(".autocomplete .ac-label")?.textContent).toBe("Template: Daily"));
    textarea.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Enter", code: "Enter", bubbles: true, cancelable: true,
    }));
    await vi.waitFor(() => expect(toasts().at(-1)?.message).toBe(
      "Can't insert: this page would exceed Tine-managed storage's 511-block or request-size limit. Nothing was changed."
    ));

    expect(pageByName("Shared")!.roots).toHaveLength(511);
    expect(doc.byId[host].raw).toBe("/Daily");
    expect(counts).toEqual({ publications: 0, dirty: 0, undo: 0 });
  } finally {
    dispose();
  }
});
