import { facetsOf } from "../render/facets";
import type { Format } from "../render/ast";
import { isAggregateFn, type AggregateFn } from "./aggregate";

export type SheetView = "table" | "grid" | "board";

export interface SheetConfig {
  view: SheetView | null;
  groupBy: string | null;
  header: boolean;
  colWidths: ReadonlyMap<number, number>;
  colAggregates: ReadonlyMap<string, AggregateFn>;
}

const VIEWS = new Set<SheetView>(["table", "grid", "board"]);

function parseColWidths(value: string): ReadonlyMap<number, number> {
  const out = new Map<number, number>();
  for (const part of value.split(";")) {
    const m = /^\s*(\d+)\s*=\s*(\d+)\s*$/.exec(part);
    if (!m) continue;
    out.set(Number(m[1]), Number(m[2]));
  }
  return out;
}

function parseColAggregates(value: string): ReadonlyMap<string, AggregateFn> {
  const out = new Map<string, AggregateFn>();
  for (const part of value.split(";")) {
    const m = /^\s*([^=;\s][^=;]*)\s*=\s*([a-z-]+)\s*$/.exec(part);
    if (!m) continue;
    const key = m[1].trim();
    const fn = m[2].toLowerCase();
    if (key && isAggregateFn(fn)) out.set(key, fn);
  }
  return out;
}

export function serializeColWidths(widths: ReadonlyMap<number, number>): string {
  return [...widths.entries()]
    .filter(([col, px]) => Number.isInteger(col) && col >= 0 && Number.isFinite(px) && px >= 0)
    .sort(([a], [b]) => a - b)
    .map(([col, px]) => `${col}=${Math.round(px)}`)
    .join(";");
}

export function serializeColAggregates(aggregates: ReadonlyMap<string, AggregateFn>): string {
  return [...aggregates.entries()]
    .filter(([key, fn]) => key.trim() && !/[=;\n\r]/.test(key) && isAggregateFn(fn))
    .sort(([a], [b]) => {
      const ai = /^\d+$/.test(a) ? Number(a) : null;
      const bi = /^\d+$/.test(b) ? Number(b) : null;
      if (ai != null && bi != null) return ai - bi;
      if (ai != null) return -1;
      if (bi != null) return 1;
      return a.localeCompare(b);
    })
    .map(([key, fn]) => `${key}=${fn}`)
    .join(";");
}

export function sheetConfig(props: readonly [string, string][]): SheetConfig {
  let view: SheetView | null = null;
  let groupBy: string | null = null;
  let header = false;
  let colWidths: ReadonlyMap<number, number> = new Map();
  let colAggregates: ReadonlyMap<string, AggregateFn> = new Map();

  for (const [rawKey, rawValue] of props) {
    const key = rawKey.trim().toLowerCase();
    const value = rawValue.trim();
    if (key === "tine.view") {
      const lower = value.toLowerCase();
      view = VIEWS.has(lower as SheetView) ? (lower as SheetView) : null;
    } else if (key === "tine.group-by") {
      groupBy = value || null;
    } else if (key === "tine.header") {
      header = value.toLowerCase() === "true";
    } else if (key === "tine.col-widths") {
      colWidths = parseColWidths(value);
    } else if (key === "tine.col-aggregates") {
      colAggregates = parseColAggregates(value);
    }
  }

  return { view, groupBy, header, colWidths, colAggregates };
}

/** Sheet config straight from a block's raw text, through the ONE block-property
 *  recognizer (`facetsOf`, lsdoc-backed + memoized) — never a second `key::` /
 *  drawer line scanner here (a duplicate recognizer drifts: fence-awareness,
 *  org drawer edge cases). */
export function sheetConfigFromRaw(raw: string, format: Format): SheetConfig {
  return sheetConfig(facetsOf(raw, format).properties);
}
