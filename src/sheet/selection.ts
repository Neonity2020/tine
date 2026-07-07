import { createRoot, createSignal } from "solid-js";
import { doc, clearSelection, selectBlock, prevVisible, nextVisible, blockIsGridView, withUndoUnit } from "../store";
import { endEdit, startEditing } from "../editorController";
import { isBuiltinHidden, splitProps } from "../editor/properties";
import {
  registerEditingStartListener,
  registerModeResetListener,
  registerOutlineSelectionListener,
} from "../modeHooks";
import { buildMatrix, type MatrixCell, type SheetMatrix } from "./matrix";
import type { SheetCellCtx } from "./context";
import { deleteColumn, deleteRow, insertColumn, insertRow, materializeCell } from "./mutations";

export const SEAM_STEPPING = true;

export interface CellSel extends SheetCellCtx {
  kind: "cell";
}

export interface RowSeamSel {
  kind: "row-seam";
  gridId: string;
  col: number;
  at: number;
}

export interface ColSeamSel {
  kind: "col-seam";
  gridId: string;
  row: number;
  at: number;
}

export type SheetSel = CellSel | RowSeamSel | ColSeamSel;
type CellSelInput = SheetCellCtx | CellSel;
type SheetSelInput = SheetSel | CellSelInput;

interface CellSelectionHooks {
  clearOutlineSelection: () => void;
  endActiveEdit: () => void;
}

const [activeCellSel, writeCellSel] = createRoot(() => createSignal<SheetSel | null>(null));
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

export function cellOwner(sel: SheetCellCtx): string {
  return `sheet:${sel.gridId}:${sel.row}:${sel.col}`;
}

export function cellSurfaceKey(gridId: string): string {
  return `sheet:${gridId}`;
}

function isCellSel(sel: SheetSel | null): sel is CellSel {
  return sel?.kind === "cell";
}

function normalizeSel(sel: SheetSelInput): SheetSel {
  if ("kind" in sel && sel.kind) return { ...sel } as SheetSel;
  return { kind: "cell", gridId: sel.gridId, row: sel.row, col: sel.col };
}

export function cellSel(): SheetSel | null {
  return activeCellSel();
}

export function setCellSel(sel: SheetSelInput | null): void {
  if (sel) {
    const normalized = normalizeSel(sel);
    if (normalized.kind === "cell") lastByGrid.set(normalized.gridId, { row: normalized.row, col: normalized.col });
    hooks.clearOutlineSelection();
    hooks.endActiveEdit();
    writeCellSel(normalized);
    return;
  }
  writeCellSel(null);
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

export function cellAt(sel: CellSelInput): MatrixCell | null {
  const matrix = matrixForGrid(sel.gridId);
  return matrix.cells.find((cell) => cell.row === sel.row && cell.col === sel.col) ?? null;
}

export function cellBlockId(sel: CellSelInput): string | null {
  return cellAt(sel)?.blockId ?? null;
}

function clampCell(gridId: string, wanted: { row: number; col: number }): CellSel | null {
  const bounds = boundsForGrid(gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return null;
  return {
    kind: "cell",
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

export function startCellEditing(sel: CellSelInput, offset?: number): boolean {
  const blockId = cellBlockId(sel);
  if (!blockId) return false;
  const node = doc.byId[blockId];
  if (!node) return false;
  const visibleLen = splitProps(node.raw, isBuiltinHidden).visible.length;
  setCellSel({ kind: "cell", gridId: sel.gridId, row: sel.row, col: sel.col });
  startEditing(blockId, offset ?? visibleLen, cellOwner(sel));
  return true;
}

export function selectCellAfterEdit(sel: SheetCellCtx): void {
  endEdit("select-block");
  setCellSel(sel);
}

type CellDirection = "up" | "down" | "left" | "right";

function flowOutVertical(sel: { gridId: string }, dir: "up" | "down"): boolean {
  clearCellSelectionOnly();
  const target = dir === "up" ? prevVisible(sel.gridId) : nextVisible(sel.gridId);
  selectBlock(target ?? sel.gridId);
  return true;
}

function exitLeft(sel: { gridId: string }): boolean {
  clearCellSelectionOnly();
  selectBlock(sel.gridId);
  return true;
}

function setClampedCell(gridId: string, row: number, col: number): boolean {
  const next = clampCell(gridId, { row, col });
  if (!next) return false;
  setCellSel(next);
  return true;
}

function moveFromRowSeam(sel: RowSeamSel, dir: CellDirection): boolean {
  const bounds = boundsForGrid(sel.gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return false;

  if (dir === "up") {
    if (sel.at <= 0) return flowOutVertical(sel, "up");
    return setClampedCell(sel.gridId, sel.at - 1, sel.col);
  }
  if (dir === "down") {
    if (sel.at >= bounds.rows) return flowOutVertical(sel, "down");
    return setClampedCell(sel.gridId, sel.at, sel.col);
  }
  if (dir === "left") {
    setCellSel({ ...sel, col: Math.max(0, sel.col - 1) });
    return true;
  }
  setCellSel({ ...sel, col: Math.min(bounds.cols - 1, sel.col + 1) });
  return true;
}

function moveFromColSeam(sel: ColSeamSel, dir: CellDirection): boolean {
  const bounds = boundsForGrid(sel.gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return false;

  if (dir === "left") {
    if (sel.at <= 0) return exitLeft(sel);
    return setClampedCell(sel.gridId, sel.row, sel.at - 1);
  }
  if (dir === "right") {
    if (sel.at >= bounds.cols) return true;
    return setClampedCell(sel.gridId, sel.row, sel.at);
  }
  if (dir === "up") {
    setCellSel({ ...sel, row: Math.max(0, sel.row - 1) });
    return true;
  }
  setCellSel({ ...sel, row: Math.min(bounds.rows - 1, sel.row + 1) });
  return true;
}

export function moveCellSelectionFrom(sel: SheetSel, dir: CellDirection): boolean {
  if (sel.kind === "row-seam") return moveFromRowSeam(sel, dir);
  if (sel.kind === "col-seam") return moveFromColSeam(sel, dir);

  const bounds = boundsForGrid(sel.gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return false;

  if (dir === "up") {
    if (SEAM_STEPPING) {
      setCellSel({ kind: "row-seam", gridId: sel.gridId, at: sel.row, col: sel.col });
      return true;
    }
    if (sel.row <= 0) return flowOutVertical(sel, "up");
    setCellSel({ ...sel, row: sel.row - 1 });
    return true;
  }
  if (dir === "down") {
    if (SEAM_STEPPING) {
      setCellSel({ kind: "row-seam", gridId: sel.gridId, at: sel.row + 1, col: sel.col });
      return true;
    }
    if (sel.row >= bounds.rows - 1) return flowOutVertical(sel, "down");
    setCellSel({ ...sel, row: sel.row + 1 });
    return true;
  }
  if (dir === "left") {
    if (SEAM_STEPPING) {
      setCellSel({ kind: "col-seam", gridId: sel.gridId, at: sel.col, row: sel.row });
      return true;
    }
    if (sel.col <= 0) return exitLeft(sel);
    setCellSel({ ...sel, col: sel.col - 1 });
    return true;
  }
  if (SEAM_STEPPING) {
    setCellSel({ kind: "col-seam", gridId: sel.gridId, at: sel.col + 1, row: sel.row });
    return true;
  }
  if (sel.col >= bounds.cols - 1) return true;
  setCellSel({ ...sel, col: sel.col + 1 });
  return true;
}

function moveCellTab(sel: SheetSel, dir: 1 | -1): boolean {
  if (!isCellSel(sel)) return true;
  const bounds = boundsForGrid(sel.gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return false;
  const next = sel.row * bounds.cols + sel.col + dir;
  if (next < 0 || next >= bounds.rows * bounds.cols) return true;
  setCellSel({
    kind: "cell",
    gridId: sel.gridId,
    row: Math.floor(next / bounds.cols),
    col: next % bounds.cols,
  });
  return true;
}

export function moveCellAfterEdit(sel: SheetCellCtx, dir: CellDirection | "tab-forward" | "tab-back"): void {
  endEdit("select-block");
  const cell: CellSel = { kind: "cell", ...sel };
  if (dir === "tab-forward") moveCellTab(cell, 1);
  else if (dir === "tab-back") moveCellTab(cell, -1);
  else moveCellSelectionFrom(cell, dir);
}

function cellElement(sel: CellSelInput): HTMLElement | null {
  if (typeof document === "undefined") return null;
  const esc = (value: string) =>
    typeof CSS !== "undefined" && CSS.escape ? CSS.escape(value) : value.replace(/["\\]/g, "\\$&");
  return document.querySelector(
    `.sheet-cell[data-sheet-grid-id="${esc(sel.gridId)}"][data-row="${sel.row}"][data-col="${sel.col}"]`
  );
}

function replaceThroughMountedEditor(sel: CellSelInput, text: string): void {
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
  if (!cellBlockId(sel)) {
    const made = materializeCell(sel.gridId, sel.row, sel.col);
    if (!made) return true;
  }
  if (!startCellEditing(sel, 0)) return true;
  replaceThroughMountedEditor(sel, text);
  return true;
}

function printableKey(e: KeyboardEvent): string | null {
  if (e.ctrlKey || e.metaKey || e.altKey || e.isComposing) return null;
  return e.key.length === 1 ? e.key : null;
}

function pageForGrid(gridId: string): string | null {
  return doc.byId[gridId]?.page ?? null;
}

function seamInsertTarget(sel: RowSeamSel | ColSeamSel): CellSel | null {
  const page = pageForGrid(sel.gridId);
  if (!page) return null;
  let target: CellSel | null = null;
  withUndoUnit("sheet:seam-insert", [page], () => {
    if (sel.kind === "row-seam") {
      const rowId = insertRow(sel.gridId, sel.at);
      if (!rowId) return;
      const col = Math.max(0, sel.col);
      if (!materializeCell(sel.gridId, sel.at, col)) return;
      target = { kind: "cell", gridId: sel.gridId, row: sel.at, col };
      return;
    }
    insertColumn(sel.gridId, sel.at);
    const row = Math.max(0, sel.row);
    if (!materializeCell(sel.gridId, row, sel.at)) return;
    target = { kind: "cell", gridId: sel.gridId, row, col: sel.at };
  });
  return target;
}

function editInsertedFromSeam(sel: RowSeamSel | ColSeamSel, text: string | null): boolean {
  const target = seamInsertTarget(sel);
  if (!target) return true;
  if (!startCellEditing(target, 0)) return true;
  if (text !== null) replaceThroughMountedEditor(target, text);
  return true;
}

function nearestAfterRowDelete(gridId: string, deletedRow: number, col: number): CellSel | null {
  const bounds = boundsForGrid(gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return null;
  return {
    kind: "cell",
    gridId,
    row: Math.max(0, Math.min(deletedRow, bounds.rows - 1)),
    col: Math.max(0, Math.min(col, bounds.cols - 1)),
  };
}

function nearestAfterColumnDelete(gridId: string, row: number, deletedCol: number): CellSel | null {
  const bounds = boundsForGrid(gridId);
  if (bounds.rows <= 0 || bounds.cols <= 0) return null;
  return {
    kind: "cell",
    gridId,
    row: Math.max(0, Math.min(row, bounds.rows - 1)),
    col: Math.max(0, Math.min(deletedCol, bounds.cols - 1)),
  };
}

function deleteFromSeam(sel: RowSeamSel | ColSeamSel, side: "before" | "after"): boolean {
  if (sel.kind === "row-seam") {
    const row = side === "before" ? sel.at - 1 : sel.at;
    if (row < 0 || row >= rowsForGrid(sel.gridId).length) return true;
    deleteRow(sel.gridId, row);
    const next = nearestAfterRowDelete(sel.gridId, row, sel.col);
    if (next) setCellSel(next);
    else {
      clearCellSelectionOnly();
      selectBlock(sel.gridId);
    }
    return true;
  }

  const col = side === "before" ? sel.at - 1 : sel.at;
  const bounds = boundsForGrid(sel.gridId);
  if (col < 0 || col >= bounds.cols) return true;
  deleteColumn(sel.gridId, col);
  const next = nearestAfterColumnDelete(sel.gridId, sel.row, col);
  if (next) setCellSel(next);
  else {
    clearCellSelectionOnly();
    selectBlock(sel.gridId);
  }
  return true;
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
  if (plain && !isCellSel(sel) && e.key === "Backspace") return deleteFromSeam(sel, "before");
  if (plain && !isCellSel(sel) && e.key === "Delete") return deleteFromSeam(sel, "after");
  if (plain && (e.key === "Enter" || e.key === "F2")) {
    if (isCellSel(sel)) startCellEditing(sel);
    else editInsertedFromSeam(sel, null);
    return true;
  }
  if (!e.ctrlKey && !e.metaKey && !e.altKey && (e.key === "Tab" || e.code === "Tab")) {
    return moveCellTab(sel, e.shiftKey ? -1 : 1);
  }

  const ch = printableKey(e);
  if (ch) return isCellSel(sel) ? overtypeCell(sel, ch) : editInsertedFromSeam(sel, ch);
  return false;
}

registerOutlineSelectionListener(() => clearCellSelectionOnly());
registerEditingStartListener((_id, owner) => {
  if (!isSheetCellOwner(owner)) clearCellSelectionOnly();
});
registerModeResetListener(() => resetCellSelectionForTests());
