import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { initParser } from "../render/parse";
import { backend } from "../backend";
import { doc, resetStore, setDoc } from "../store";
import { closeExportModal, exportModal } from "../ui";
import { clearTransientLayersForTest } from "../transientLayers";
import { resetReferenceSectionState } from "../referenceSectionState";
import { LinkedReferences } from "./LinkedReferences";
import { UnlinkedReferences } from "./UnlinkedReferences";
import { ExportModal } from "./ExportModal";
import type { ExportNode } from "../editor/exportText";
import type { RefGroup } from "../types";

// GH #348: Linked References and Unlinked References get independent explicit
// batch export. A quiet chooser (all selected by default; uncheck to subset)
// feeds the SAME Copy / export modal the multiselect-block gesture uses — the
// node payload is grouped by source page, and normal page export never learns
// about references.

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  closeExportModal();
  clearTransientLayersForTest();
  resetStore();
  resetReferenceSectionState();
  document.body.innerHTML = "";
  localStorage.clear();
  vi.restoreAllMocks();
});

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

async function settle() {
  await tick();
  await tick();
  await tick();
}

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

const LINKED: RefGroup[] = [
  {
    page: "Note A",
    kind: "page",
    blocks: [
      { id: "a1", raw: "alpha mentions [[Target]]", collapsed: false, children: [
        { id: "a1c", raw: "alpha child detail", collapsed: false, children: [] },
      ] },
      { id: "a2", raw: "second link to [[Target]]", collapsed: false, children: [] },
    ],
  },
  {
    page: "Note B",
    kind: "page",
    blocks: [{ id: "b1", raw: "beta links [[Target]] too", collapsed: false, children: [] }],
  },
];

const UNLINKED: RefGroup[] = [
  {
    page: "Loose",
    kind: "page",
    blocks: [{ id: "u1", raw: "loose plain-text mention", collapsed: false, children: [] }],
  },
];

function chooser(): HTMLElement | null {
  return document.querySelector<HTMLElement>(".ref-export-chooser");
}

function chooserRows(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>(".ref-export-row")];
}

function chooserRow(text: string): HTMLElement | undefined {
  return chooserRows().find((row) => row.textContent?.includes(text));
}

function exportButton(container: HTMLElement, label: string): HTMLButtonElement {
  const btn = [...container.querySelectorAll<HTMLButtonElement>("button")].find(
    (b) => b.getAttribute("aria-label") === label
  );
  expect(btn, `export button ${label}`).toBeTruthy();
  return btn!;
}

function chooserButton(label: string): HTMLButtonElement {
  const btn = [...chooser()!.querySelectorAll<HTMLButtonElement>("button")].find(
    (b) => b.textContent?.trim() === label
  );
  expect(btn, `chooser button ${label}`).toBeTruthy();
  return btn!;
}

function nodesRequest(): { nodes: ExportNode[]; count: number } {
  const req = exportModal();
  expect(req, "export modal request").not.toBeNull();
  expect(typeof req === "object" && "nodes" in req!, "request carries nodes, not store ids").toBe(true);
  return req as { nodes: ExportNode[]; count: number };
}

describe("batch export of Linked References (GH #348)", () => {
  it("chooser lists every linked entry pre-selected; a subset flows into the same Copy / export modal", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(LINKED);
    const savePage = vi.spyOn(backend(), "savePage");
    const m = mount(() => (<><LinkedReferences name="Target" /><ExportModal /></>) as JSX.Element);
    try {
      await settle();
      exportButton(m.root, "Copy / export linked references").click();
      await settle();

      const box = chooser();
      expect(box).not.toBeNull();
      expect(box!.textContent).toContain("Linked References");
      const rows = chooserRows();
      expect(rows).toHaveLength(3);
      expect(rows.every((row) => row.querySelector<HTMLInputElement>("input")!.checked)).toBe(true);
      expect(box!.textContent).toContain("3 of 3");

      // Subset: keep one entry of Note A, drop the rest.
      chooserRow("second link")!.querySelector<HTMLInputElement>("input")!.click();
      chooserRow("beta links")!.querySelector<HTMLInputElement>("input")!.click();
      await settle();
      expect(box!.textContent).toContain("1 of 3");

      chooserButton("Copy / export…").click();
      await settle();

      expect(chooser()).toBeNull(); // chooser hands over to the export modal
      const req = nodesRequest();
      expect(req.count).toBe(1);
      // Grouped by source page: Note B is gone entirely, Note A keeps one child
      // entry, and the kept entry carries its subtree.
      expect(req.nodes.map((n) => n.raw)).toEqual(["Note A"]);
      expect(req.nodes[0].children.map((c) => c.raw)).toEqual(["alpha mentions [[Target]]"]);
      expect(req.nodes[0].children[0].children.map((c) => c.raw)).toEqual(["alpha child detail"]);

      // The SAME modal (formats/options) previews the payload. Default export
      // options are persisted-clean: switch to source view to see raw text.
      const markdown = [...document.querySelectorAll<HTMLButtonElement>(".export-indent-btn")]
        .find((b) => b.textContent?.trim() === "Markdown");
      markdown!.click();
      await settle();
      const preview = document.querySelector<HTMLTextAreaElement>(".export-preview")!.value;
      expect(preview).toContain("alpha mentions [[Target]]");
      expect(preview).toContain("alpha child detail");
      expect(preview).toContain("Note A");
      expect(preview).not.toContain("second link");
      expect(preview).not.toContain("beta links");
      expect(savePage).not.toHaveBeenCalled();
    } finally {
      closeExportModal();
      m.dispose();
    }
  });

  it("All / None manage the whole section at once and an empty selection cannot export", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(LINKED);
    const m = mount(() => (<><LinkedReferences name="Target" /><ExportModal /></>) as JSX.Element);
    try {
      await settle();
      exportButton(m.root, "Copy / export linked references").click();
      await settle();

      expect(chooserButton("Copy / export…").disabled).toBe(false);
      chooserButton("None").click();
      await settle();
      expect(chooser()!.textContent).toContain("0 of 3");
      expect(chooserButton("Copy / export…").disabled).toBe(true);

      chooserButton("All").click();
      await settle();
      expect(chooser()!.textContent).toContain("3 of 3");
      expect(chooserButton("Copy / export…").disabled).toBe(false);

      chooserButton("Copy / export…").click();
      await settle();
      const req = nodesRequest();
      expect(req.count).toBe(3);
      expect(req.nodes.map((n) => n.raw)).toEqual(["Note A", "Note B"]);
      expect(req.nodes[0].children).toHaveLength(2);
      expect(req.nodes[1].children).toHaveLength(1);
    } finally {
      closeExportModal();
      m.dispose();
    }
  });

  it("preserves an Org source page's syntax for both its group and blocks", async () => {
    setDoc({
      byId: {},
      pages: [{ name: "Note A", kind: "page", title: "Note A", preBlock: null, roots: [], format: "org", readOnly: false, guide: false }],
      feed: [],
      loaded: true,
    });
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([LINKED[0]]);
    const m = mount(() => (<><LinkedReferences name="Target" /><ExportModal /></>) as JSX.Element);
    try {
      await settle();
      exportButton(m.root, "Copy / export linked references").click();
      await settle();
      chooserButton("Copy / export…").click();
      await settle();

      const req = nodesRequest();
      expect(req.nodes[0].format).toBe("org");
      expect(req.nodes[0].children.every((node) => node.format === "org")).toBe(true);
      expect([...document.querySelectorAll<HTMLButtonElement>(".export-indent-btn")]
        .some((button) => button.textContent?.trim() === "Org")).toBe(true);
    } finally {
      closeExportModal();
      m.dispose();
    }
  });

  it("Cancel closes the chooser without opening the export modal or touching the graph", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(LINKED);
    const savePage = vi.spyOn(backend(), "savePage");
    const rawBefore = new Map(Object.entries(doc.byId).map(([id, n]) => [id, n.raw]));
    const m = mount(() => (<><LinkedReferences name="Target" /><ExportModal /></>) as JSX.Element);
    try {
      await settle();
      exportButton(m.root, "Copy / export linked references").click();
      await settle();
      chooserRow("beta links")!.querySelector<HTMLInputElement>("input")!.click();
      await settle();

      chooserButton("Cancel").click();
      await settle();
      expect(chooser()).toBeNull();
      expect(exportModal()).toBeNull();
      expect(savePage).not.toHaveBeenCalled();
      expect(new Map(Object.entries(doc.byId).map(([id, n]) => [id, n.raw]))).toEqual(rawBefore);

      // Overlay-click cancellation behaves the same.
      exportButton(m.root, "Copy / export linked references").click();
      await settle();
      chooser()!.closest(".modal-overlay")!.dispatchEvent(
        new MouseEvent("click", { bubbles: true, cancelable: true })
      );
      await settle();
      expect(chooser()).toBeNull();
      expect(exportModal()).toBeNull();
    } finally {
      m.dispose();
    }
  });
});

describe("batch export of Unlinked References (GH #348)", () => {
  it("is independent: the unlinked chooser lists unlinked entries only", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(LINKED);
    vi.spyOn(backend(), "getUnlinkedRefs").mockResolvedValue(UNLINKED);
    const m = mount(() => (<><UnlinkedReferences name="Target" /><ExportModal /></>) as JSX.Element);
    try {
      m.root.querySelector<HTMLElement>(".references-header")!.click();
      await settle();
      exportButton(m.root, "Copy / export unlinked references").click();
      await settle();

      const box = chooser();
      expect(box).not.toBeNull();
      expect(box!.textContent).toContain("Unlinked References");
      const rows = chooserRows();
      expect(rows).toHaveLength(1);
      expect(rows[0].textContent).toContain("loose plain-text mention");
      expect(box!.textContent).not.toContain("alpha mentions");

      chooserButton("Copy / export…").click();
      await settle();
      const req = nodesRequest();
      expect(req.count).toBe(1);
      expect(req.nodes.map((n) => n.raw)).toEqual(["Loose"]);
      expect(req.nodes[0].children.map((c) => c.raw)).toEqual(["loose plain-text mention"]);
    } finally {
      closeExportModal();
      m.dispose();
    }
  });
});
