import { createRoot, createSignal } from "solid-js";
import { doc, clearSelection, selectBlock, prevVisible, nextVisible, blockIsGridView } from "../store";
import { endEdit, startEditing } from "../editorController";
import { isBuiltinHidden, splitProps } from "../editor/properties";
import {
  registerEditingStartListener,
  registerModeResetListener,
  registerOutlineSelectionListener,
} from "../modeHooks";
import { buildMatrix, type MatrixCell, type SheetMatrix } from "./matrix";
import type { SheetCellCtx } from "./context";

export interface CellSel {
  gridId: string;
  row: number;
  col: number;
}

interface CellSelectionHooks {
  clearOutlineSelection: () => void;
  endActiveEdit: () => void;
}

const [activeCellSel, writeCellSel] = createRoot(() => createSignal<CellSel | null>(null));
const lastByGrid = new Map<string, { row: number; col: number }>();

let hooks: CellSelectionHooks = {
  clearOutlineSelection: clearSelection,
  endActiveEdit: () => endEdit("select-block"),
};

export function installCellSelectionHooks(next: Partial<CellSelectionHooks>): () => void {
  const prev = hooks;
  hooks = { ...hooks, ...next };
  return () => {
    hooks = prev;
  };
}

function clearCellSelectionOnly(): void {
  writeCellSel(null);
}

export function resetCellSelectionForTests(): void {
  clearCellSelectionOnly();
  lastByGrid.clear();
}

export function isSheetCellOwner(owner: string | null): boolean {
  return owner?.startsWith("sheet:") ?? false;
}

export function cellOwner(sel: CellSel): string {
  return `sheet:${sel.gridId}:${sel.row}:${sel.col}`;
}

export function cellSurfaceKey(gridId: string): string {
  return `sheet:${gridId}`;
}

export function cellSel(): CellSel | null {
  return activeCellSel();
}

export function setCellSel(sel: CellSel | null): void {
  if (sel) {
    lastByGrid.set(sel.gridId, { row: sel.row, col: sel.col });
    hooks.clearOutlineSelection();
    hooks.endActiveEdit();
  }
  writeCellSel(sel ? { ...sel } : null);
}

export function lastCellFor(gridId: string): { row: number; col: number } | null {
  const last = lastByGrid.get(gridId);
  return last ? { ...last } : null;
}

function rowsForGrid(gridId: string): { id: string; cellIds: readonly string[] }[] {
  return (doc.byId[gridId]?.children ?? []).map((id) => ({
    id,
    cellIds: doc.byId[id]?.children ?? [],
  }));
}

export function matrixForGrid(gridId: string): SheetMatrix {
  return buildMatrix(rowsForGrid(gridId));
}

function boundsForGrid(gridId: string): { rows: number; cols: number } {
  const matrix = matrixForGrid(gridId);
  return { rows: matrix.rows, cols: matrix.rows === 0 ? 0 : matrix.cols };
}

export function cellAt(sel: CellSel): MatrixCell | null {
  const matrix = matrixForGrid(sel.gridId);
  return matrix.cells.find((cell) => cell.row === sel.row && cell.col === sel.col) ?? null;
}

export function cellBlockId(sel: CellSel): string | null {
  return cellAt(sel)?.blockId ?? null;
}

function clampCell(gridId: string, wanted: { row: number; col: number }): CellSel | null {
  const bounds = boundsForGrid(gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return null;
  return {
    gridId,
    row: Math.max(0, Math.min(wanted.row, bounds.rows - 1)),
    col: Math.max(0, Math.min(wanted.col, bounds.cols - 1)),
  };
}

export function enterGridSelection(gridId: string): boolean {
  if (!blockIsGridView(gridId)) return false;
  const target = clampCell(gridId, lastCellFor(gridId) ?? { row: 0, col: 0 });
  if (!target) return false;
  setCellSel(target);
  return true;
}

export function startCellEditing(sel: CellSel, offset?: number): boolean {
  const blockId = cellBlockId(sel);
  if (!blockId) return false;
  const node = doc.byId[blockId];
  if (!node) return false;
  const visibleLen = splitProps(node.raw, isBuiltinHidden).visible.length;
  setCellSel(sel);
  startEditing(blockId, offset ?? visibleLen, cellOwner(sel));
  return true;
}

export function selectCellAfterEdit(sel: SheetCellCtx): void {
  endEdit("select-block");
  setCellSel(sel);
}

type CellDirection = "up" | "down" | "left" | "right";

function flowOutVertical(sel: CellSel, dir: "up" | "down"): boolean {
  clearCellSelectionOnly();
  const target = dir === "up" ? prevVisible(sel.gridId) : nextVisible(sel.gridId);
  selectBlock(target ?? sel.gridId);
  return true;
}

export function moveCellSelectionFrom(sel: CellSel, dir: CellDirection): boolean {
  const bounds = boundsForGrid(sel.gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return false;

  if (dir === "up") {
    if (sel.row <= 0) return flowOutVertical(sel, "up");
    setCellSel({ ...sel, row: sel.row - 1 });
    return true;
  }
  if (dir === "down") {
    if (sel.row >= bounds.rows - 1) return flowOutVertical(sel, "down");
    setCellSel({ ...sel, row: sel.row + 1 });
    return true;
  }
  if (dir === "left") {
    if (sel.col <= 0) {
      clearCellSelectionOnly();
      selectBlock(sel.gridId);
      return true;
    }
    setCellSel({ ...sel, col: sel.col - 1 });
    return true;
  }
  if (sel.col >= bounds.cols - 1) return true;
  setCellSel({ ...sel, col: sel.col + 1 });
  return true;
}

function moveCellTab(sel: CellSel, dir: 1 | -1): boolean {
  const bounds = boundsForGrid(sel.gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return false;
  const next = sel.row * bounds.cols + sel.col + dir;
  if (next < 0 || next >= bounds.rows * bounds.cols) return true;
  setCellSel({
    gridId: sel.gridId,
    row: Math.floor(next / bounds.cols),
    col: next % bounds.cols,
  });
  return true;
}

export function moveCellAfterEdit(sel: SheetCellCtx, dir: CellDirection | "tab-forward" | "tab-back"): void {
  endEdit("select-block");
  if (dir === "tab-forward") moveCellTab(sel, 1);
  else if (dir === "tab-back") moveCellTab(sel, -1);
  else moveCellSelectionFrom(sel, dir);
}

function cellElement(sel: CellSel): HTMLElement | null {
  if (typeof document === "undefined") return null;
  const esc = (value: string) =>
    typeof CSS !== "undefined" && CSS.escape ? CSS.escape(value) : value.replace(/["\\]/g, "\\$&");
  return document.querySelector(
    `.sheet-cell[data-sheet-grid-id="${esc(sel.gridId)}"][data-row="${sel.row}"][data-col="${sel.col}"]`
  );
}

function replaceThroughMountedEditor(sel: CellSel, text: string): void {
  const apply = () => {
    const textarea = cellElement(sel)?.querySelector("textarea.block-editor") as HTMLTextAreaElement | null;
    if (!textarea) return false;
    textarea.value = text;
    textarea.setSelectionRange(text.length, text.length);
    let ev: Event;
    try {
      ev = new InputEvent("input", { bubbles: true, inputType: "insertText", data: text });
    } catch {
      ev = new Event("input", { bubbles: true });
    }
    textarea.dispatchEvent(ev);
    return true;
  };
  queueMicrotask(() => {
    if (apply()) return;
    if (typeof requestAnimationFrame === "function") requestAnimationFrame(() => apply());
    else setTimeout(() => apply(), 0);
  });
}

function overtypeCell(sel: CellSel, text: string): boolean {
  if (!cellBlockId(sel)) return true;
  if (!startCellEditing(sel, 0)) return true;
  replaceThroughMountedEditor(sel, text);
  return true;
}

function printableKey(e: KeyboardEvent): string | null {
  if (e.ctrlKey || e.metaKey || e.altKey || e.isComposing) return null;
  return e.key.length === 1 ? e.key : null;
}

export function handleCellSelectionKey(e: KeyboardEvent): boolean {
  const sel = cellSel();
  if (!sel) return false;
  const plain = !e.ctrlKey && !e.metaKey && !e.altKey;

  if (plain && e.key === "Escape") {
    clearCellSelectionOnly();
    selectBlock(sel.gridId);
    return true;
  }
  if (plain && e.key === "ArrowUp") return moveCellSelectionFrom(sel, "up");
  if (plain && e.key === "ArrowDown") return moveCellSelectionFrom(sel, "down");
  if (plain && e.key === "ArrowLeft") return moveCellSelectionFrom(sel, "left");
  if (plain && e.key === "ArrowRight") return moveCellSelectionFrom(sel, "right");
  if (plain && (e.key === "Enter" || e.key === "F2")) {
    startCellEditing(sel);
    return true;
  }
  if (!e.ctrlKey && !e.metaKey && !e.altKey && (e.key === "Tab" || e.code === "Tab")) {
    return moveCellTab(sel, e.shiftKey ? -1 : 1);
  }

  const ch = printableKey(e);
  if (ch) return overtypeCell(sel, ch);
  return false;
}

registerOutlineSelectionListener(() => clearCellSelectionOnly());
registerEditingStartListener((_id, owner) => {
  if (!isSheetCellOwner(owner)) clearCellSelectionOnly();
});
registerModeResetListener(() => resetCellSelectionForTests());
