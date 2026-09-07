// The one summary a {{query}} result renders, for every inline view.
//
// `(aggregate …)` / `(group-by …)` ride in the DSL and are parse-but-ignored by
// the Rust engine, which returns the full block set; the math is computed here
// in the frontend from the returned rows. Kept DOM-free and unit-testable.
//
// This file used to expose `foldAggregate` + `groupRows`, a single-aggregate
// fold over a `{page, props}` shadow row. Both are gone: a view carries an
// ORDERED LIST of aggregates, and folding only `aggregates[0]` silently dropped
// every other column its author asked for, while the shadow row's own property
// lookup could disagree with the board rendered directly beneath it. One
// summary over the caller's real rows replaces both.

/** The three functions a QUERY aggregate can name (contract §4). The sheet's
 *  own seventeen-name footer vocabulary is a different, wider set, and is
 *  deliberately not reachable from here. */
export type QueryAggFn = "count" | "sum" | "avg";

/** One `tine.col-aggregates` entry as the QUERY reads it (contract §4): a bare
 *  `count` has an empty field and means the whole-result count. */
export type QueryAggregateEntry = readonly [field: string, fn: QueryAggFn];

export interface QuerySummaryCell {
  text: string;
  /** Rows that could not contribute — sum/avg over an absent or non-numeric
   *  value. Always 0 for a count. */
  skipped: number;
}

export interface QuerySummaryGroup {
  /** The group's own key; `null` for the rows that carry no value. */
  key: string | null;
  label: string;
  count: number;
  /** One cell per requested aggregate, in the requested order. */
  cells: QuerySummaryCell[];
}

export interface QuerySummary {
  /** One column per REQUESTED aggregate, in the requested order — repeats and a
   *  bare whole-result count included, because the view carries a LIST. */
  columns: { label: string; entry: QueryAggregateEntry }[];
  overall: QuerySummaryCell[];
  /** `null` when the result is not grouped. */
  groups: QuerySummaryGroup[] | null;
  /** The grouping field's label, for the breakdown's first column head. */
  groupLabel: string | null;
  /** Whether one row can sit in SEVERAL groups (tags), so the surface can say
   *  so rather than imply the groups partition the result. */
  multiMembership: boolean;
}

/** The user-facing name of one aggregate column. */
export function queryAggregateLabel([field, fn]: QueryAggregateEntry): string {
  const verb = fn === "count" ? "Count" : fn === "sum" ? "Sum" : "Avg";
  return field ? `${verb} of ${field}` : verb;
}

/** **The one summary a query result renders.**
 *
 *  Grouping and value reading are the CALLER's, deliberately. The rows are the
 *  same result DTOs the Board renders, and their group keys come from the one
 *  shared reader (`sheet/fields.ts::groupKeysForBlock`) rather than a second
 *  property lookup that could disagree with the face beside it.
 *
 *  The numbers are unchanged: count, numeric sum, numeric average parsed with
 *  `parseFloat` (so "3 hrs" contributes 3), rounded to three decimals, with
 *  non-contributing rows counted as `skipped`. Nothing new is invented here.
 *
 *  Returns `null` when there is nothing to summarize. */
export function querySummary<R>(input: {
  rows: readonly R[];
  aggregates: readonly QueryAggregateEntry[];
  /** `null` means "not grouped". Otherwise the groups this row belongs to; an
   *  empty list reads as the single `null` group, as the Board does. */
  groupKeys: ((row: R) => readonly (string | null)[]) | null;
  groupLabel?: string | null;
  multiMembership?: boolean;
  /** The row's value for an aggregate's field, as text. */
  value: (row: R, field: string) => string | null | undefined;
}): QuerySummary | null {
  const aggregates = [...input.aggregates];
  const keysOf = input.groupKeys;
  if (!aggregates.length && !keysOf) return null;
  const fold = (set: readonly R[], [field, fn]: QueryAggregateEntry): QuerySummaryCell => {
    if (fn === "count") return { text: `${set.length}`, skipped: 0 };
    let sum = 0;
    let n = 0;
    let skipped = 0;
    for (const row of set) {
      const parsed = parseFloat((input.value(row, field) ?? "").trim());
      if (Number.isFinite(parsed)) {
        sum += parsed;
        n++;
      } else skipped++;
    }
    const val = fn === "sum" ? sum : n ? sum / n : 0;
    // Round to 3 decimals to avoid float noise; integers print without a dot.
    return { text: `${Math.round(val * 1000) / 1000}`, skipped };
  };
  const columns = aggregates.map((entry) => ({ label: queryAggregateLabel(entry), entry }));
  const overall = aggregates.map((entry) => fold(input.rows, entry));
  if (!keysOf) {
    return { columns, overall, groups: null, groupLabel: null, multiMembership: false };
  }
  // First-seen key first, which is the order the Board's own columns appear in.
  const buckets = new Map<string, { key: string | null; rows: R[] }>();
  for (const row of input.rows) {
    const keys = keysOf(row);
    for (const key of keys.length ? keys : [null]) {
      // Prefixed so a group literally named like the none-marker cannot collide.
      const id = key === null ? "!none" : `k${key}`;
      const bucket = buckets.get(id);
      if (bucket) bucket.rows.push(row);
      else buckets.set(id, { key, rows: [row] });
    }
  }
  return {
    columns,
    overall,
    groups: [...buckets.values()].map((bucket) => ({
      key: bucket.key,
      label: bucket.key ?? "(none)",
      count: bucket.rows.length,
      cells: aggregates.map((entry) => fold(bucket.rows, entry)),
    })),
    groupLabel: input.groupLabel ?? null,
    multiMembership: input.multiMembership ?? false,
  };
}
