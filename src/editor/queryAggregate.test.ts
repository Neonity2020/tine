import { describe, it, expect } from "vitest";
import { querySummary, queryAggregateLabel, type QueryAggregateEntry } from "./queryAggregate";

interface Row {
  page: string;
  props: Record<string, string>;
}

const rows: Row[] = [
  { page: "A", props: { hours: "2", status: "open" } },
  { page: "A", props: { hours: "3.5", status: "open" } },
  { page: "B", props: { hours: "not a number", status: "done" } },
  { page: "B", props: { status: "done" } }, // no hours at all
];

const value = (row: Row, field: string) => (field === "page" ? row.page : row.props[field]);
const by = (field: string) => (row: Row) => [value(row, field) ?? null];

const summarize = (
  set: readonly Row[],
  aggregates: readonly QueryAggregateEntry[],
  group?: string,
) =>
  querySummary<Row>({
    rows: set,
    aggregates,
    groupKeys: group ? by(group) : null,
    groupLabel: group ?? null,
    value,
  });

// The arithmetic these cases pin is the arithmetic `foldAggregate` shipped; the
// summary took over its callers, not its numbers.
describe("the aggregate arithmetic", () => {
  it("count ignores properties and counts rows", () => {
    expect(summarize(rows, [["", "count"]])!.overall).toEqual([{ text: "4", skipped: 0 }]);
    expect(summarize(rows, [["hours", "count"]])!.overall).toEqual([{ text: "4", skipped: 0 }]);
  });

  it("sum adds numeric values and skips non-numeric / absent", () => {
    // 2 + 3.5 = 5.5; the "not a number" and the missing-hours rows are skipped.
    expect(summarize(rows, [["hours", "sum"]])!.overall).toEqual([{ text: "5.5", skipped: 2 }]);
  });

  it("avg divides by the count of numeric contributors, not the row count", () => {
    // (2 + 3.5) / 2 = 2.75, NOT / 4.
    expect(summarize(rows, [["hours", "avg"]])!.overall).toEqual([{ text: "2.75", skipped: 2 }]);
  });

  it("avg of an all-non-numeric set is 0 with everything skipped (no NaN)", () => {
    const r: Row[] = [{ page: "X", props: { hours: "x" } }];
    expect(summarize(r, [["hours", "avg"]])!.overall).toEqual([{ text: "0", skipped: 1 }]);
  });

  it("parseFloat is lenient: a trailing unit still contributes its leading number", () => {
    const r: Row[] = [{ page: "X", props: { hours: "3 hrs" } }];
    expect(summarize(r, [["hours", "sum"]])!.overall).toEqual([{ text: "3", skipped: 0 }]);
  });

  it("rounds float noise to 3 decimals", () => {
    const r: Row[] = [
      { page: "X", props: { n: "0.1" } },
      { page: "X", props: { n: "0.2" } },
    ];
    expect(summarize(r, [["n", "sum"]])!.overall).toEqual([{ text: "0.3", skipped: 0 }]);
  });
});

describe("the grouped breakdown", () => {
  it("groups by page, preserving first-seen order", () => {
    const s = summarize(rows, [["", "count"]], "page")!;
    expect(s.groups!.map((g) => [g.key, g.cells[0].text])).toEqual([
      ["A", "2"],
      ["B", "2"],
    ]);
  });

  it("groups by a property, bucketing absent values under (none)", () => {
    const s = summarize(rows, [["", "count"]], "status")!;
    expect(s.groups!.map((g) => [g.label, g.cells[0].text])).toEqual([
      ["open", "2"],
      ["done", "2"],
    ]);
  });

  it("a missing property value falls into its own (none) group", () => {
    const s = summarize([{ page: "X", props: {} }], [["", "count"]], "status")!;
    expect(s.groups!.map((g) => [g.key, g.label])).toEqual([[null, "(none)"]]);
  });

  it("a group literally named like the none-marker stays distinct from it", () => {
    const r: Row[] = [{ page: "X", props: { status: "(none)" } }, { page: "X", props: {} }];
    const s = summarize(r, [["", "count"]], "status")!;
    expect(s.groups!.map((g) => g.key)).toEqual(["(none)", null]);
  });

  it("carries the group label and reports single membership by default", () => {
    const s = summarize(rows, [["", "count"]], "status")!;
    expect(s.groupLabel).toBe("status");
    expect(s.multiMembership).toBe(false);
  });
});

// This is the defect the summary exists to fix: the view carries an ordered
// LIST of aggregates, and the old renderer folded only `aggregates[0]`.
describe("every requested aggregate", () => {
  const many: QueryAggregateEntry[] = [
    ["", "count"],
    ["hours", "sum"],
    ["hours", "avg"],
    ["hours", "sum"],
  ];

  it("renders in the requested order, repeats and the keyless count included", () => {
    const s = summarize(rows, many)!;
    expect(s.columns.map((c) => c.label)).toEqual([
      "Count",
      "Sum of hours",
      "Avg of hours",
      "Sum of hours",
    ]);
    expect(s.overall.map((c) => c.text)).toEqual(["4", "5.5", "2.75", "5.5"]);
  });

  it("gives every group one cell per requested aggregate, in the same order", () => {
    const s = summarize(rows, many, "status")!;
    expect(s.groups!.map((g) => g.cells.map((c) => c.text))).toEqual([
      ["2", "5.5", "2.75", "5.5"],
      ["2", "0", "0", "0"],
    ]);
  });

  it("labels a keyless entry as the whole-result count", () => {
    expect(queryAggregateLabel(["", "count"])).toBe("Count");
    expect(queryAggregateLabel(["hours", "avg"])).toBe("Avg of hours");
  });
});

describe("multi-membership grouping", () => {
  // Tags place one row in every tag's group, exactly as the Board does, so the
  // group counts deliberately do not partition the result.
  const tagged: Row[] = [
    { page: "A", props: { hours: "2" } },
    { page: "B", props: { hours: "3" } },
  ];
  const tags = new Map([
    [tagged[0], ["work", "urgent"]],
    [tagged[1], ["work"]],
  ]);

  it("counts a row in every group it belongs to and says so", () => {
    const s = querySummary<Row>({
      rows: tagged,
      aggregates: [["", "count"], ["hours", "sum"]],
      groupKeys: (row) => tags.get(row) ?? [],
      groupLabel: "Tags",
      multiMembership: true,
      value,
    })!;
    expect(s.groups!.map((g) => [g.key, g.count])).toEqual([
      ["work", 2],
      ["urgent", 1],
    ]);
    expect(s.overall[0].text).toBe("2"); // the result itself still has two rows
    expect(s.multiMembership).toBe(true);
  });

  it("reads an empty key list as the single (none) group", () => {
    const s = querySummary<Row>({
      rows: tagged,
      aggregates: [["", "count"]],
      groupKeys: () => [],
      value,
    })!;
    expect(s.groups!.map((g) => [g.key, g.count])).toEqual([[null, 2]]);
  });
});

describe("nothing to summarize", () => {
  it("is null when there is neither an aggregate nor a grouping", () => {
    expect(querySummary<Row>({ rows, aggregates: [], groupKeys: null, value })).toBeNull();
  });

  it("is still a breakdown when a grouping asks for no aggregate", () => {
    const s = summarize(rows, [], "status")!;
    expect(s.columns).toEqual([]);
    expect(s.groups!.map((g) => [g.key, g.count])).toEqual([
      ["open", 2],
      ["done", 2],
    ]);
  });
});
