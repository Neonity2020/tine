import {
  blockIsGridView,
  blockProperty,
  deleteBlock,
  doc,
  formatForBlock,
  insertEmptyChildBlock,
  setBlockProperty,
  withUndoUnit,
} from "../store";
import { serializeColWidths, sheetConfigFromRaw } from "./config";

function gridRows(gridId: string): string[] | null {
  if (!blockIsGridView(gridId)) return null;
  return doc.byId[gridId]?.children ?? null;
}

function gridPage(gridId: string): string | null {
  return doc.byId[gridId]?.page ?? null;
}

function colCount(rows: readonly string[]): number {
  if (rows.length === 0) return 0;
  let cols = 1;
  for (const rowId of rows) cols = Math.max(cols, doc.byId[rowId]?.children.length ?? 0);
  return cols;
}

function colWidths(gridId: string): ReadonlyMap<number, number> {
  const node = doc.byId[gridId];
  return node ? sheetConfigFromRaw(node.raw, formatForBlock(gridId)).colWidths : new Map();
}

function writeColWidths(gridId: string, widths: ReadonlyMap<number, number>): void {
  const serialized = serializeColWidths(widths);
  const current = blockProperty(gridId, "tine.col-widths");
  if (serialized === "") {
    if (current !== null) setBlockProperty(gridId, "tine.col-widths", null);
    return;
  }
  if (current === serialized) return;
  setBlockProperty(gridId, "tine.col-widths", serialized);
}

function shiftedForInsert(widths: ReadonlyMap<number, number>, at: number): Map<number, number> {
  const next = new Map<number, number>();
  for (const [col, px] of widths) next.set(col >= at ? col + 1 : col, px);
  return next;
}

function shiftedForDelete(widths: ReadonlyMap<number, number>, col: number): Map<number, number> {
  const next = new Map<number, number>();
  for (const [idx, px] of widths) {
    if (idx === col) continue;
    next.set(idx > col ? idx - 1 : idx, px);
  }
  return next;
}

export function insertRow(gridId: string, at: number): string | null {
  const rows = gridRows(gridId);
  const page = gridPage(gridId);
  if (!rows || !page || at < 0 || at > rows.length) return null;
  return withUndoUnit("sheet:insert-row", [page], () => insertEmptyChildBlock(gridId, at));
}

export function deleteRow(gridId: string, row: number): void {
  const rows = gridRows(gridId);
  const page = gridPage(gridId);
  if (!rows || !page || row < 0 || row >= rows.length) return;
  withUndoUnit("sheet:delete-row", [page], () => deleteBlock(rows[row]));
}

export function insertColumn(gridId: string, at: number): void {
  const rows = gridRows(gridId);
  const page = gridPage(gridId);
  if (!rows || !page) return;
  const cols = colCount(rows);
  if (at < 0 || at > cols) return;
  withUndoUnit("sheet:insert-column", [page], () => {
    for (const rowId of rows) {
      const row = doc.byId[rowId];
      if (row && row.children.length >= at) insertEmptyChildBlock(rowId, at);
    }
    writeColWidths(gridId, shiftedForInsert(colWidths(gridId), at));
  });
}

export function deleteColumn(gridId: string, col: number): void {
  const rows = gridRows(gridId);
  const page = gridPage(gridId);
  if (!rows || !page) return;
  const cols = colCount(rows);
  if (col < 0 || col >= cols) return;
  withUndoUnit("sheet:delete-column", [page], () => {
    for (const rowId of rows) {
      const cellId = doc.byId[rowId]?.children[col];
      if (cellId) deleteBlock(cellId);
    }
    writeColWidths(gridId, shiftedForDelete(colWidths(gridId), col));
  });
}

export function materializeCell(gridId: string, row: number, col: number): string | null {
  const rows = gridRows(gridId);
  const page = gridPage(gridId);
  if (!rows || !page || row < 0 || row >= rows.length || col < 0) return null;
  const rowId = rows[row];
  const existing = doc.byId[rowId]?.children[col];
  if (existing) return existing;
  return withUndoUnit("sheet:materialize-cell", [page], () => {
    let made: string | null = null;
    while ((doc.byId[rowId]?.children.length ?? 0) <= col) {
      made = insertEmptyChildBlock(rowId, doc.byId[rowId]?.children.length ?? 0);
      if (!made) return null;
    }
    return doc.byId[rowId]?.children[col] ?? made;
  });
}

export function setColumnWidth(gridId: string, col: number, px: number | null): void {
  const rows = gridRows(gridId);
  const page = gridPage(gridId);
  if (!rows || !page) return;
  const cols = colCount(rows);
  if (col < 0 || col >= cols) return;
  withUndoUnit("sheet:resize-column", [page], () => {
    const next = new Map(colWidths(gridId));
    if (px === null) next.delete(col);
    else next.set(col, Math.max(40, Math.round(px)));
    writeColWidths(gridId, next);
  });
}
