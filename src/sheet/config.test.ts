import { beforeAll, describe, expect, it } from "vitest";
import { serializeColAggregates, serializeColWidths, sheetConfig, sheetConfigFromRaw } from "./config";
import { initParser } from "../render/parse";

// sheetConfigFromRaw reads properties through the one lsdoc-backed recognizer
// (facetsOf), which needs the wasm parser initialized.
beforeAll(() => initParser());

describe("sheetConfig", () => {
  it("reads grid view, header, and positional column widths", () => {
    const cfg = sheetConfig([
      ["tine.view", "grid"],
      ["tine.header", "true"],
      ["tine.col-widths", "0=120;2=200"],
      ["tine.col-aggregates", "0=sum;2=average"],
    ]);

    expect(cfg.view).toBe("grid");
    expect(cfg.header).toBe(true);
    expect(Array.from(cfg.colWidths.entries())).toEqual([
      [0, 120],
      [2, 200],
    ]);
    expect(Array.from(cfg.colAggregates.entries())).toEqual([
      ["0", "sum"],
      ["2", "average"],
    ]);
  });

  it("matches keys and boolean values case-insensitively", () => {
    const cfg = sheetConfig([
      ["Tine.View", "BOARD"],
      ["TINE.HEADER", "TRUE"],
    ]);

    expect(cfg.view).toBe("board");
    expect(cfg.header).toBe(true);
  });

  it("degrades unknown view values to plain outline", () => {
    expect(sheetConfig([["tine.view", "calendar"]]).view).toBe(null);
    expect(sheetConfig([["tine.view", "list"]]).view).toBe(null);
  });

  it("skips malformed width entries without throwing", () => {
    const cfg = sheetConfig([
      ["tine.col-widths", "0=120; nope; 2 = 88 ; -1=10; 3=wide; 4=0"],
      ["tine.col-aggregates", "0=sum; bad ; prop:qty = median ; 2=nope"],
    ]);

    expect(Array.from(cfg.colWidths.entries())).toEqual([
      [0, 120],
      [2, 88],
      [4, 0],
    ]);
    expect(Array.from(cfg.colAggregates.entries())).toEqual([
      ["0", "sum"],
      ["prop:qty", "median"],
    ]);
  });

  it("serializes positional column widths in the parser-owned grammar", () => {
    expect(serializeColWidths(new Map([[2, 88], [0, 120]]))).toBe("0=120;2=88");
    expect(serializeColWidths(new Map([[1, 40.4], [-1, 20], [3, Number.NaN]]))).toBe("1=40");
  });

  it("serializes column aggregates in the parser-owned grammar", () => {
    expect(serializeColAggregates(new Map([["prop:qty", "sum"], ["state", "checked"], ["0", "average"]]))).toBe(
      "0=average;prop:qty=sum;state=checked"
    );
    expect(serializeColAggregates(new Map([["bad;key", "sum"], ["1", "not-real" as never]]))).toBe("");
  });

  it("ignores non-tine properties", () => {
    const cfg = sheetConfig([
      ["title", "Project"],
      ["query-table", "true"],
    ]);

    expect(cfg.view).toBe(null);
    expect(cfg.header).toBe(false);
    expect(cfg.colWidths.size).toBe(0);
    expect(cfg.colAggregates.size).toBe(0);
  });

  it("reads sheet config directly from md properties and org drawers", () => {
    expect(sheetConfigFromRaw("Grid\ntine.view:: grid", "md").view).toBe("grid");
    expect(sheetConfigFromRaw("Grid\ntine.col-aggregates:: prop:qty=sum", "md").colAggregates.get("prop:qty")).toBe("sum");
    expect(
      sheetConfigFromRaw("Grid\n:PROPERTIES:\n:tine.view: grid\n:tine.header: true\n:END:", "org")
    ).toMatchObject({ view: "grid", header: true });
  });
});
