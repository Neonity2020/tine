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
  canonicalGroupField,
  isLegacyBareColumnList,
  legacyGroupField,
  mergeQueryAggregateValue,
  queryColumnTokens,
  queryDisplayPropertyWrites,
  queryViewPropertyPatch,
  resolveQueryColumns,
  resolveQueryGrouping,
  retainedQueryAggregateSegments,
  selectedQueryColumns,
  type PropertyPairs,
  type QueryColumnsResolution,
  type QueryGroupingResolution,
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

interface GroupingCase {
  why: string;
  properties: [string, string][];
  parsed?: { group_by?: string; view?: string };
  resolution: QueryGroupingResolution;
}

const GROUPING_FIXTURES: GroupingCase[] = JSON.parse(
  readFileSync(
    new URL("../../crates/tine-core/tests/fixtures/query-grouping/resolution.json", import.meta.url),
    "utf8",
  ),
);

describe("the shared grouping resolver (P5B)", () => {
  it("matches the Rust resolver on every shared fixture", () => {
    expect(GROUPING_FIXTURES.length).toBeGreaterThan(20);
    for (const item of GROUPING_FIXTURES) {
      expect(
        resolveQueryGrouping(item.properties, item.parsed ?? {}),
        `${item.why} — ${JSON.stringify(item.properties)}`,
      ).toEqual(item.resolution);
    }
  });

  it("keeps the three outcomes distinct: a field, an explicit clear, and unset", () => {
    const kinds = new Set(GROUPING_FIXTURES.map((item) => item.resolution.kind));
    expect(kinds).toEqual(new Set(["field", "cleared", "unset"]));
  });

  it("tells the task marker from an ordinary property named state", () => {
    // The one ambiguity the new key exists to end. Both spellings, one wire.
    expect(resolveQueryGrouping([["tine.group-field", "state"]])).toEqual({
      kind: "field",
      field: "state",
    });
    expect(resolveQueryGrouping([["tine.group-field", "prop:state"]])).toEqual({
      kind: "field",
      field: "prop:state",
    });
    // And a property literally NAMED `prop:state` is reachable too, with no new
    // escaping rule in the property bytes.
    expect(canonicalGroupField("prop:prop:state")).toBe("prop:prop:state");
  });

  it("captures a legacy token's meaning at the view the note is persisted with", () => {
    // The same bytes, two readings — which is exactly why they cannot both keep
    // living on one bare token.
    expect(legacyGroupField("state", true)).toBe("state");
    expect(legacyGroupField("state", false)).toBe("prop:state");
    // The defect the sheet-face reading fixes: `status` used to fall through
    // `isFieldId` and silently become the task marker.
    expect(legacyGroupField("status", true)).toBe("prop:status");
    // The list reading keeps a literal `prop:` prefix inside the property KEY.
    expect(legacyGroupField("prop:state", false)).toBe("prop:prop:state");
  });

  it("treats a present-but-unreadable new key as an explicit clear, with no fallback", () => {
    for (const value of ["", "   ", "status", "prop:", "formula:"]) {
      expect(
        resolveQueryGrouping(
          [
            ["tine.group-field", value],
            ["tine.group-by", "state"],
          ],
          { group_by: "cost" },
        ),
        value,
      ).toEqual({ kind: "cleared" });
    }
  });
});

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
      // A legacy token, untouched. On a TABLE face it resolves to the ordinary
      // property `status`, and the destination view is the same table — so
      // there is nothing to canonicalize and nothing to write (P5B).
      ["tine.group-by", "status"],
      ["tine.sample", "20"],
      ["tine.columns", "a;b"],
      ["tine.col-aggregates", "count;hours=sum"],
    ];
    expect(queryViewPropertyPatch({
      view: {
        view: "table",
        sort: [["a", "desc"]],
        group_by: "prop:status",
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
    expect(patch({ group_by: "prop:status", aggregates: [["", "count"]] })).toEqual({
      "tine.group-field": "prop:status",
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
      "tine.group-field": "page",
      "tine.sample": "20",
      "tine.columns": "a",
      "tine.col-aggregates": "count",
    });
  });

  it("removes a stale property when the setting is cleared", () => {
    // The grouping clear is spelled differently from the others on purpose: the
    // new key stays PRESENT and empty, because "no grouping" has to outlive a
    // view switch, and the recognized legacy key is retired in the same patch.
    expect(patch({}, [["tine.sort", "a desc"], ["tine.group-by", "status"]])).toEqual({
      "tine.sort": null,
      "tine.group-field": null,
      "tine.group-by": null,
    });
    expect(patch({ group_by: "" }, [["tine.group-by", "status"]])).toEqual({
      "tine.group-field": "",
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

describe("clearing a column selection", () => {
  /** The writer removes `tine.columns` rather than emptying it, while the
   *  resolver distinguishes a PRESENT-but-empty value from an absent one. That
   *  asymmetry only matters if something behind the key can come back — so this
   *  states, for the two things that could:
   *
   *   * the legacy `tine.fields` branch, which the same patch retires under
   *     exactly the condition that branch would have read it;
   *   * the query TEXT's own columns, which do not exist: no parser produces
   *     `ViewSettings.columns` and no printer emits one (`query/print.rs::
   *     og_view`), so `Unset` leaves an empty list either way.
   *
   *  Grouping is the opposite case and is written as the EMPTY value, because
   *  the legacy key and the DSL directive behind it really can come back. */
  it("removes the key, and nothing behind it comes back", () => {
    const properties: PropertyPairs = [
      ["tine.columns", "cost;status"],
      // A proven pre-split bare list — the one thing the legacy branch reads.
      ["tine.fields", "cost;status"],
    ];
    const writes = queryDisplayPropertyWrites({
      before: { columns: ["cost", "status"] },
      view: { columns: [] },
      properties,
    });
    expect(writes).toEqual([
      ["tine.columns", null],
      ["tine.fields", null],
    ]);
    // The block as it stands after those writes.
    expect(resolveQueryColumns([])).toEqual({ kind: "unset" });
    expect(selectedQueryColumns([])).toBeNull();
    // …which draws the same thing an explicit empty value does. `cleared` and
    // `unset` differ in what they suppress, and there is nothing here to
    // suppress.
    expect(selectedQueryColumns([["tine.columns", ""]])).toBeNull();
  });

  it("keeps a TYPED schema, which the legacy branch would not have read anyway", () => {
    // `tine.fields:: cost=number` is a schema, not a column list. It survives
    // the clear (I-4) — and it cannot resurrect as columns, because the branch
    // that would read it applies the same grammar that rejects it.
    const properties: PropertyPairs = [
      ["tine.columns", "cost"],
      ["tine.fields", "cost=number"],
    ];
    expect(
      queryDisplayPropertyWrites({ before: { columns: ["cost"] }, view: { columns: [] }, properties }),
    ).toEqual([["tine.columns", null]]);
    expect(resolveQueryColumns([["tine.fields", "cost=number"]])).toEqual({ kind: "unset" });
  });
});

describe("a DISPLAY edit's write set", () => {
  const writes = (before: ViewSettings, view: ViewSettings, properties: PropertyPairs = []) =>
    Object.fromEntries(
      queryDisplayPropertyWrites({ before, view, properties }).map(([k, v]) => [k, v]),
    );

  it("states only the fact the edit changed", () => {
    // FAIL-BEFORE (I-20): the engine re-reads asynchronously, so a second edit
    // made inside one parse round-trip still carries the reading that predates
    // the first. Here the grouping was just saved and the panel's reading has
    // not caught up; the wide save baseline would call that a disagreement and
    // REMOVE the grouping the user set a moment ago.
    const stale: ViewSettings = { view: "table" };
    const properties: PropertyPairs = [
      ["tine.view", "table"],
      ["tine.group-field", "prop:status"],
    ];
    expect(writes(stale, { ...stale, columns: ["cost"] }, properties)).toEqual({
      "tine.columns": "cost",
    });
  });

  it("pins a legacy grouping when the VIEW changes, even though the grouping did not", () => {
    // The one fact a view switch must state even when its own value is
    // unchanged: a bare `tine.group-by` means different things at different
    // views, which is the whole reason the canonical key exists.
    const before: ViewSettings = { group_by: "prop:state" };
    expect(
      writes(before, { ...before, view: "board" }, [["tine.group-by", "state"]]),
    ).toEqual({
      "tine.view": "board",
      "tine.group-field": "prop:state",
      "tine.group-by": null,
    });
  });

  it("leaves an untouched legacy key alone on an unrelated edit", () => {
    // I-4: a column edit is not a licence to rewrite how a note spells its
    // grouping. The legacy token still resolves the same way at the same view.
    expect(
      writes({ group_by: "prop:status" }, { group_by: "prop:status", columns: ["cost"] }, [
        ["tine.group-by", "status"],
      ]),
    ).toEqual({ "tine.columns": "cost" });
  });

  it("does not re-state a canonical grouping the reading has not caught up to", () => {
    // FAIL-BEFORE: the view-switch exemption existed for the LEGACY token, which
    // reinterprets across views. `tine.group-field` does not — so once it is on
    // the block, restating it from a stale reading is the same clobber, made by
    // the exemption itself. Here the grouping was just saved and the very next
    // click switches the view.
    const stale: ViewSettings = { view: "board" };
    expect(
      writes(stale, { ...stale, view: "table" }, [
        ["tine.view", "board"],
        ["tine.group-field", "prop:status"],
      ]),
    ).toEqual({ "tine.view": "table" });
  });

  it("writes nothing when the edit changed nothing", () => {
    const view: ViewSettings = { view: "board", group_by: "state" };
    expect(queryDisplayPropertyWrites({ before: view, view, properties: [] })).toEqual([]);
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

  // **What the merge keeps, the editor has to be able to SHOW** (P5B §5).
  //
  // Preserving a table-only segment byte for byte and then rendering a panel
  // that lists only the three the query understands is a silent retention: the
  // author sees `estimate=median` vanish from every surface and has no way to
  // know their note still carries it. The panel reads the same segments through
  // the same parser — there is exactly one — and states them as kept.
  describe("the segments the panel reports as retained", () => {
    it("names every segment the query reader does not own, in order", () => {
      expect(retainedQueryAggregateSegments([
        ["tine.col-aggregates", " count ; estimate=median ;hours=sum; owner=distinct"],
      ])).toEqual(["estimate=median", "owner=distinct"]);
    });

    it("reports nothing when every segment is the query's own", () => {
      expect(retainedQueryAggregateSegments([["tine.col-aggregates", "count;hours=avg"]])).toEqual([]);
      expect(retainedQueryAggregateSegments([])).toEqual([]);
      expect(retainedQueryAggregateSegments([["tine.col-aggregates", ""]])).toEqual([]);
    });

    it("reports exactly what an unrelated edit preserves, so the two cannot drift", () => {
      const raw = "count;estimate=median;hours=sum";
      const retained = retainedQueryAggregateSegments([["tine.col-aggregates", raw]]);
      // The same edit the panel makes when it changes an aggregate it DOES own.
      const merged = mergeQueryAggregateValue(raw, [["", "count"], ["hours", "avg"]])!;
      for (const segment of retained) expect(merged.split(";")).toContain(segment);
    });
  });
});
