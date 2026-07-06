import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startEditing, endEdit } from "../editorController";
import { selectBlock } from "../store";
import {
  cellSel,
  installCellSelectionHooks,
  lastCellFor,
  resetCellSelectionForTests,
  setCellSel,
} from "./selection";

let disposeHooks: (() => void) | null = null;

beforeEach(() => {
  resetCellSelectionForTests();
});

afterEach(() => {
  disposeHooks?.();
  disposeHooks = null;
  resetCellSelectionForTests();
  endEdit("blur");
});

describe("cell selection state", () => {
  it("sets and clears the active cell while remembering the last cell per grid", () => {
    const clearOutlineSelection = vi.fn();
    const endActiveEdit = vi.fn();
    disposeHooks = installCellSelectionHooks({ clearOutlineSelection, endActiveEdit });

    setCellSel({ gridId: "grid-a", row: 1, col: 2 });

    expect(cellSel()).toEqual({ gridId: "grid-a", row: 1, col: 2 });
    expect(lastCellFor("grid-a")).toEqual({ row: 1, col: 2 });
    expect(clearOutlineSelection).toHaveBeenCalledTimes(1);
    expect(endActiveEdit).toHaveBeenCalledTimes(1);

    setCellSel({ gridId: "grid-b", row: 0, col: 0 });
    setCellSel(null);

    expect(cellSel()).toBeNull();
    expect(lastCellFor("grid-a")).toEqual({ row: 1, col: 2 });
    expect(lastCellFor("grid-b")).toEqual({ row: 0, col: 0 });
  });

  it("outline selection clears cell selection through the transition hook", () => {
    disposeHooks = installCellSelectionHooks({
      clearOutlineSelection: () => {},
      endActiveEdit: () => {},
    });
    setCellSel({ gridId: "grid-a", row: 0, col: 1 });

    selectBlock("outline-block");

    expect(cellSel()).toBeNull();
  });

  it("non-cell editing clears cell selection, while sheet-cell editing keeps it", () => {
    disposeHooks = installCellSelectionHooks({
      clearOutlineSelection: () => {},
      endActiveEdit: () => {},
    });

    setCellSel({ gridId: "grid-a", row: 0, col: 1 });
    startEditing("cell-block", 0, "sheet:grid-a:0:1");
    expect(cellSel()).toEqual({ gridId: "grid-a", row: 0, col: 1 });

    startEditing("outline-block", 0, null);
    expect(cellSel()).toBeNull();
  });
});
