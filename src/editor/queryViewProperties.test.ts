// The TypeScript half of the query display-settings agreement (P5A).
//
// Two things are proved here:
//
//  1. **The column resolver matches Rust**, case for case, against the SHARED
//     fixture set `crates/tine-core/tests/fixtures/query-columns/resolution.json`
//     — the same file `crates/tine-core/tests/query_columns_resolution.rs`
//     reads. A published page and the app must not disagree about which columns
//     a note selects, and the only way two implementations stay honest is one
//     corpus.
//  2. **The patch writes what a save must write and nothing else.** The baseline
//     is the block's currently PERSISTED properties, never a "user touched this
//     control" flag — because the OG printer drops grouping and aggregates on
//     reprint, so an unrelated filter edit is exactly when those facts need
//     materializing.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  isLegacyBareColumnList,
  mergeQueryAggregateValue,
  queryColumnTokens,
  queryViewPropertyPatch,
  resolveQueryColumns,
  selectedQueryColumns,
  type PropertyPairs,
  type QueryColumnsResolution,
} from "./queryViewProperties";
import type { ViewSettings } from "./queryIr";

interface Case {
  why: string;
  properties: [string, string][];
  resolution: QueryColumnsResolution;
}

const FIXTURES: Case[] = JSON.parse(
  readFileSync(
    new URL("../../crates/tine-core/tests/fixtures/query-columns/resolution.json", import.meta.url),
    "utf8",
  ),
);

describe("the shared visible-column resolver", () => {
  it("matches the Rust resolver on every shared fixture", () => {
    expect(FIXTURES.length).toBeGreaterThan(10);
    for (const item of FIXTURES) {
      expect(resolveQueryColumns(item.properties), `${item.why} — ${JSON.stringify(item.properties)}`)
        .toEqual(item.resolution);
    }
  });

  it("keeps the three outcomes distinct: named, explicitly cleared, and unset", () => {
    const kinds = new Set(FIXTURES.map((item) => item.resolution.kind));
    expect(kinds).toEqual(new Set(["named", "cleared", "unset"]));
  });

  it("invalidates a whole list on one bad token rather than half-reading it", () => {
    expect(queryColumnTokens("a;b;c")).toEqual(["a", "b", "c"]);
    expect(queryColumnTokens(" a ; ; b ")).toEqual(["a", "b"]);
    expect(queryColumnTokens("")).toEqual([]);
    for (const bad of ["a;cost=number", "a;b\rc", "a;b\nc", "a;b\0c"]) {
      expect(queryColumnTokens(bad), bad).toBeNull();
    }
  });

  it("renders no selection for both cleared and unset, which is the DEFAULT column set", () => {
    expect(selectedQueryColumns([["tine.columns", "a;b"]])).toEqual(["a", "b"]);
    expect(selectedQueryColumns([["tine.columns", "   "], ["tine.fields", "a;b"]])).toBeNull();
    expect(selectedQueryColumns([["tine.fields", "cost=number"]])).toBeNull();
  });

  it("recognises a pre-split bare list without ever treating a typed schema as one", () => {
    expect(isLegacyBareColumnList("page;status")).toBe(true);
    expect(isLegacyBareColumnList("cost=number")).toBe(false);
    expect(isLegacyBareColumnList("page;cost=number")).toBe(false);
    expect(isLegacyBareColumnList("")).toBe(false);
    expect(isLegacyBareColumnList(null)).toBe(false);
  });
});

describe("the lossless query view-property patch", () => {
  const patch = (view: ViewSettings, properties: PropertyPairs = []) =>
    Object.fromEntries(queryViewPropertyPatch({ view, properties }).map(([k, v]) => [k, v]));

  it("writes nothing when every persisted fact already says what the view says", () => {
    const properties: PropertyPairs = [
      ["tine.view", "table"],
      ["tine.sort", "a desc"],
      ["tine.group-by", "status"],
      ["tine.sample", "20"],
      ["tine.columns", "a;b"],
      ["tine.col-aggregates", "count;hours=sum"],
    ];
    expect(queryViewPropertyPatch({
      view: {
        view: "table",
        sort: [["a", "desc"]],
        group_by: "status",
        sample: 20,
        columns: ["a", "b"],
        aggregates: [["", "count"], ["hours", "sum"]],
      },
      properties,
    })).toEqual([]);
  });

  it("materializes an OG-only grouping and aggregate on a FILTER-only save", () => {
    // The reprint is about to drop them: `og_view` re-emits only `(sort-by …)`
    // and `(sample …)`. Nothing about the view "changed" — only the property
    // baseline can tell you the facts are about to be lost.
    expect(patch({ group_by: "status", aggregates: [["", "count"]] })).toEqual({
      "tine.group-by": "status",
      "tine.col-aggregates": "count",
    });
  });

  it("materializes the whole effective view when the block crosses to TQL", () => {
    // TQL text carries no directives at all, so every fact the properties do not
    // already spell has to be written in the same undo unit (§4.3 Y2).
    expect(patch({
      view: "table",
      sort: [["updated", "desc"]],
      group_by: "page",
      sample: 20,
      columns: ["a"],
      aggregates: [["", "count"]],
    })).toEqual({
      "tine.view": "table",
      "tine.sort": "updated desc",
      "tine.group-by": "page",
      "tine.sample": "20",
      "tine.columns": "a",
      "tine.col-aggregates": "count",
    });
  });

  it("removes a stale property when the setting is cleared", () => {
    expect(patch({}, [["tine.sort", "a desc"], ["tine.group-by", "status"]])).toEqual({
      "tine.sort": null,
      "tine.group-by": null,
    });
  });

  it("never writes or deletes the typed schema, the widths, the filter, or an unknown key", () => {
    const properties: PropertyPairs = [
      ["tine.fields", "cost=number;severity=text"],
      ["tine.table-widths", "cost=120"],
      ["tine.col-widths", "0=100"],
      ["tine.header", "true"],
      ["tine.filter", "cost > 1"],
      ["tine.formula.effort", "cost * 2"],
      ["something-else", "kept"],
    ];
    const written = queryViewPropertyPatch({ view: { sort: [["a", "asc"]] }, properties });
    expect(written).toEqual([["tine.sort", "a asc"]]);
  });

  it("does not rewrite a persisted fact merely because its spelling differs", () => {
    expect(queryViewPropertyPatch({
      view: { sort: [["a", "desc"]], aggregates: [["", "count"], ["hours", "sum"]] },
      properties: [["tine.sort", "  a   desc  "], ["tine.col-aggregates", " count ; hours = sum "]],
    })).toEqual([]);
  });

  it("leaves an unrelated filter edit on a pre-split note alone, legacy list included", () => {
    // The bare `tine.fields` list IS what this block's properties currently
    // spell for columns, so the effective columns are unchanged and there is
    // nothing to migrate.
    expect(queryViewPropertyPatch({
      view: { columns: ["page", "status"], sort: [["a", "asc"]] },
      properties: [["tine.fields", "page;status"]],
    })).toEqual([["tine.sort", "a asc"]]);
  });

  it("retires a PROVEN legacy bare list when a save states the columns", () => {
    expect(queryViewPropertyPatch({
      view: { columns: ["page"] },
      properties: [["tine.fields", "page;status"]],
    })).toEqual([["tine.columns", "page"], ["tine.fields", null]]);
  });

  it("clearing the columns retires the legacy list too, so it cannot come back", () => {
    expect(queryViewPropertyPatch({
      view: {},
      properties: [["tine.fields", "page;status"]],
    })).toEqual([["tine.columns", null], ["tine.fields", null]]);
  });

  it("keeps a typed schema when the columns change: it is not a legacy list", () => {
    expect(queryViewPropertyPatch({
      view: { columns: ["page"] },
      properties: [["tine.fields", "cost=number"]],
    })).toEqual([["tine.columns", "page"]]);
  });

  it("respects a PRESENT empty columns property: its explicit presence wins", () => {
    expect(queryViewPropertyPatch({
      view: {},
      properties: [["tine.columns", ""], ["tine.fields", "page;status"]],
    })).toEqual([]);
  });
});

describe("the aggregate segment merge", () => {
  it("preserves the raw value byte for byte when the recognized list is unchanged", () => {
    expect(mergeQueryAggregateValue(" count ; estimate=median ", [["", "count"]])).toBeUndefined();
  });

  it("edits recognized segments in place and keeps table-only ones verbatim", () => {
    // `median` is a sheet-footer function the query reader knows nothing about.
    // Rewriting the value from the query's list alone would delete it.
    expect(mergeQueryAggregateValue("count;estimate=median;hours=sum", [["", "count"], ["hours", "avg"]]))
      .toBe("count;estimate=median;hours=avg");
  });

  it("removes surplus recognized slots and appends the remaining new entries", () => {
    expect(mergeQueryAggregateValue("a=sum;b=sum;x=median", [["a", "sum"]]))
      .toBe("a=sum;x=median");
    expect(mergeQueryAggregateValue("a=sum;x=median", [["a", "sum"], ["b", "avg"], ["", "count"]]))
      .toBe("a=sum;x=median;b=avg;count");
  });

  it("keeps repeated keys and their order — a query's aggregates are a LIST", () => {
    expect(mergeQueryAggregateValue(null, [["prop:cost", "sum"], ["prop:cost", "avg"]]))
      .toBe("prop:cost=sum;prop:cost=avg");
    expect(mergeQueryAggregateValue("prop:cost=sum;prop:cost=avg", [["prop:cost", "sum"], ["prop:cost", "avg"]]))
      .toBeUndefined();
  });

  it("never deletes a value that holds only unrecognized settings", () => {
    expect(mergeQueryAggregateValue("estimate=median", [])).toBeUndefined();
    expect(mergeQueryAggregateValue("estimate=median", [["", "count"]])).toBe("estimate=median;count");
  });

  it("removes the property when the last recognized entry goes and nothing else is there", () => {
    expect(mergeQueryAggregateValue("count", [])).toBeNull();
  });
});
