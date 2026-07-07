import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { initParser } from "../render/parse";
import {
  blockProperty,
  blockSubtreeMarkdown,
  doc,
  loadSingle,
  pageToDto,
  resetStore,
  undo,
} from "../store";
import type { BlockDto, PageDto } from "../types";
import { deleteColumn, deleteRow, insertColumn, insertRow, materializeCell } from "./mutations";

let counter = 0;
function blk(raw: string, children: BlockDto[] = []): BlockDto {
  return { id: `m${counter++}`, raw, collapsed: false, children };
}

function loadGrid(raw = "Grid\ntine.view:: grid"): string {
  const grid = blk(raw, [
    blk("", [blk("A"), blk("B"), blk("C")]),
    blk("", [blk("D")]),
    blk("", []),
  ]);
  grid.properties = [["tine.view", "grid"]];
  const widths = /(?:^|\n)tine\.col-widths:: ?([^\n]*)/.exec(raw)?.[1];
  if (widths != null) grid.properties.push(["tine.col-widths", widths]);
  const dto: PageDto = { name: "Sheet", kind: "page", title: "Sheet", pre_block: null, blocks: [grid] };
  loadSingle(dto);
  return grid.id;
}

function rows(gridId: string): string[] {
  return [...doc.byId[gridId].children];
}

function rowCells(rowId: string): string[] {
  return doc.byId[rowId].children.map((id) => doc.byId[id].raw);
}

function gridShape(gridId: string): string[][] {
  return rows(gridId).map((rowId) => rowCells(rowId));
}

beforeAll(() => initParser());

beforeEach(() => {
  counter = 0;
  resetStore();
});

describe("sheet structural mutations", () => {
  it("inserts an empty row and one undo fully reverts it", () => {
    const gridId = loadGrid();
    const inserted = insertRow(gridId, 1);

    expect(inserted).toBeTruthy();
    expect(rows(gridId)).toHaveLength(4);
    expect(doc.byId[inserted!].raw).toBe("");
    expect(doc.byId[inserted!].children).toEqual([]);
    expect(blockSubtreeMarkdown(gridId, 0, true)).toContain("\t-");

    undo();
    expect(gridShape(gridId)).toEqual([["A", "B", "C"], ["D"], []]);
  });

  it("deletes a row subtree and one undo restores it", () => {
    const gridId = loadGrid();
    const deletedRow = rows(gridId)[0];
    const deletedCell = doc.byId[deletedRow].children[1];

    deleteRow(gridId, 0);

    expect(doc.byId[deletedRow]).toBeUndefined();
    expect(doc.byId[deletedCell]).toBeUndefined();
    expect(gridShape(gridId)).toEqual([["D"], []]);

    undo();
    expect(gridShape(gridId)).toEqual([["A", "B", "C"], ["D"], []]);
  });

  it("inserts a column across ragged rows and shifts col-width keys in the same undo unit", () => {
    const gridId = loadGrid("Grid\ntine.view:: grid\ntine.col-widths:: 0=120;2=88");

    insertColumn(gridId, 1);

    expect(gridShape(gridId)).toEqual([["A", "", "B", "C"], ["D", ""], []]);
    expect(blockProperty(gridId, "tine.col-widths")).toBe("0=120;3=88");

    undo();
    expect(gridShape(gridId)).toEqual([["A", "B", "C"], ["D"], []]);
    expect(blockProperty(gridId, "tine.col-widths")).toBe("0=120;2=88");
  });

  it("deletes a column across ragged rows and rewrites col-width keys in the same undo unit", () => {
    const gridId = loadGrid("Grid\ntine.view:: grid\ntine.col-widths:: 0=120;1=77;2=88");

    deleteColumn(gridId, 1);

    expect(gridShape(gridId)).toEqual([["A", "C"], ["D"], []]);
    expect(blockProperty(gridId, "tine.col-widths")).toBe("0=120;1=88");

    undo();
    expect(gridShape(gridId)).toEqual([["A", "B", "C"], ["D"], []]);
    expect(blockProperty(gridId, "tine.col-widths")).toBe("0=120;1=77;2=88");
  });

  it("materializes a hole by appending exactly the missing cells", () => {
    const gridId = loadGrid();
    const rowId = rows(gridId)[1];

    const cellId = materializeCell(gridId, 1, 3);

    expect(cellId).toBe(doc.byId[rowId].children[3]);
    expect(rowCells(rowId)).toEqual(["D", "", "", ""]);
    expect(blockSubtreeMarkdown(gridId, 0, true)).toContain("\t\t- D");

    undo();
    expect(rowCells(rowId)).toEqual(["D"]);
  });

  it("no-ops cleanly on invalid coordinates", () => {
    const gridId = loadGrid();
    const before = pageToDto("Sheet");

    expect(insertRow(gridId, -1)).toBeNull();
    deleteRow(gridId, 99);
    insertColumn(gridId, 99);
    deleteColumn(gridId, -1);
    expect(materializeCell(gridId, 99, 0)).toBeNull();

    expect(pageToDto("Sheet")).toEqual(before);
  });
});
