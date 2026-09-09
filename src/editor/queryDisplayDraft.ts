// **The query workspace's display draft: one normalizer, three callers** (P5C).
//
// A virtual query workspace (ADR 0042) is a device-local route, not a graph
// file, so the display choices a user makes inside one have nowhere to live but
// the route itself. This module owns the *shape* of that draft and nothing else:
// the router's mutation, the session serializer and the session restorer all go
// through the SAME function here, so a draft that survives a save is exactly a
// draft the router would have accepted, and vice versa.
//
// **It is pure and it is not a query parser.** No store, no DOM, no backend, and
// above all no query language: a draft carries only the non-presentation half of
// `ViewSettings`, already lifted by `query_parse`. The backend remains the only
// producer of query text (ADR 0042), and `QueryRoute.presentation` remains the
// sole authority for which view is shown — `view` is deliberately not a member
// of a draft at all.
//
// **Why a validator here rather than trust** (D-2b). A persisted session is
// content arriving from outside the running process: an older build, a
// half-written file, a hand-edited session. The in-scope scenario is a malformed
// or foreign session document, and the response is bounded and local — the draft
// is dropped, the workspace falls back to what the query text says, and nothing
// else about the route is disturbed. It is not a re-authentication of Tine's own
// state: nothing here refuses to open anything.
import type { AggFn, Field, SortDir, ViewKind, ViewSettings } from "./queryIr";
import { canonicalGroupField } from "./queryViewProperties";
import type { QueryRoute } from "../router";

/** Which source of page membership a Friendly mixed search requests: page
 *  names/aliases, contained block text, or their union. This is unrelated to
 *  the routed physical-page scope used by query execution. */
export type FriendlyPageMatchScope = "names" | "content" | "both";

const FRIENDLY_PAGE_MATCH_SCOPES: ReadonlySet<string> = new Set([
  "names",
  "content",
  "both",
]);

/** The one normalizer for route patches and persisted sessions. `null` means
 *  unreadable; callers choose whether that rejects a patch or drops one
 *  optional persisted field. */
export function normalizeFriendlyPageMatchScope(value: unknown): FriendlyPageMatchScope | null {
  return typeof value === "string" && FRIENDLY_PAGE_MATCH_SCOPES.has(value)
    ? value as FriendlyPageMatchScope
    : null;
}

/** The non-presentation half of `ViewSettings`, as a query workspace's route
 *  carries it.
 *
 *  `view` is excluded BY TYPE, not by convention: `QueryRoute.presentation` is
 *  the one place a workspace's view is stated, and a draft that could also carry
 *  one would be a second authority for the same question. */
export type QueryDisplayDraft = Omit<ViewSettings, "view">;

/** Bounds. A draft is device-local disposable state, so these are sized to keep
 *  one route small and cheap to serialize, not to express a product limit. */
export const QUERY_DISPLAY_MAX_LIST = 64;
export const QUERY_DISPLAY_MAX_FIELD = 512;
export const QUERY_DISPLAY_MAX_JSON = 65_536;

/** The largest `sample`: `ViewSettings.sample` is a `u32` at the bridge. */
export const QUERY_DISPLAY_MAX_SAMPLE = 4_294_967_295;

/** The recognized members. Anything else in an incoming object is dropped rather
 *  than carried forward: a draft is not an extension point, and a key this build
 *  cannot read is a key it cannot honour. */
const DRAFT_KEYS = ["sort", "group_by", "columns", "aggregates", "sample"] as const;

const SORT_DIRS: ReadonlySet<string> = new Set<SortDir>(["asc", "desc"]);
const AGG_FNS: ReadonlySet<string> = new Set<AggFn>(["count", "sum", "avg"]);

/** A field name inside a LIST member (`sort`, `columns`, `aggregates`).
 *
 *  The rule is the property grammar's, transcribed from the constraint that
 *  makes it a rule: `;` separates segments and `=` splits an aggregate key from
 *  its function (`view.rs::parse_col_aggregates`), CR/LF/NUL end a property
 *  line, and the reader trims each segment — so a padded name would come back
 *  naming a different field. A draft that cannot round-trip into the properties
 *  a materialized query would carry is refused here rather than silently
 *  rewritten later. Compare `fields.ts::queryAggregateFieldName`, which asks the
 *  same question of a resolved `FieldId`. */
function validListField(value: unknown): value is Field {
  return (
    typeof value === "string"
    && value.length > 0
    && value.length <= QUERY_DISPLAY_MAX_FIELD
    && value === value.trim()
    && !/[=;\0\r\n]/.test(value)
  );
}

function validTuple(value: unknown): value is [unknown, unknown] {
  return Array.isArray(value) && value.length === 2;
}

function normalizeSort(value: unknown): [Field, SortDir][] | null {
  if (!Array.isArray(value) || value.length > QUERY_DISPLAY_MAX_LIST) return null;
  const out: [Field, SortDir][] = [];
  for (const entry of value) {
    if (!validTuple(entry)) return null;
    const [field, dir] = entry;
    if (!validListField(field) || typeof dir !== "string" || !SORT_DIRS.has(dir)) return null;
    out.push([field, dir as SortDir]);
  }
  return out;
}

function normalizeColumns(value: unknown): Field[] | null {
  if (!Array.isArray(value) || value.length > QUERY_DISPLAY_MAX_LIST) return null;
  const out: Field[] = [];
  for (const entry of value) {
    if (!validListField(entry)) return null;
    out.push(entry);
  }
  return out;
}

/** Aggregates, with the one documented exception: `["", "count"]` is the
 *  fieldless whole-result count (contract §5, X3). An empty field with `sum` or
 *  `avg` names nothing to add up, so it is a malformed entry, not a count. */
function normalizeAggregates(value: unknown): [Field, AggFn][] | null {
  if (!Array.isArray(value) || value.length > QUERY_DISPLAY_MAX_LIST) return null;
  const out: [Field, AggFn][] = [];
  for (const entry of value) {
    if (!validTuple(entry)) return null;
    const [field, fn] = entry;
    if (typeof fn !== "string" || !AGG_FNS.has(fn)) return null;
    if (field === "") {
      if (fn !== "count") return null;
      out.push(["", "count"]);
      continue;
    }
    if (!validListField(field)) return null;
    out.push([field, fn as AggFn]);
  }
  return out;
}

/** The grouping member, read by the SAME helper that reads the persisted
 *  `tine.group-field` property (`canonicalGroupField`) — never by the legacy
 *  view-dependent reader, which exists only to interpret pre-P5B bytes and would
 *  turn an arbitrary word into `prop:<word>`.
 *
 *  The scalar grammar is unambiguous — a grouping value is one field, not a list
 *  — so `;` and `=` are legal INSIDE a canonical `prop:`/`formula:` name here,
 *  unlike in the list members above. The B helper still refuses CR/LF/NUL, and
 *  the length bound is applied to the value as given.
 *
 *  The empty string is the explicit clear (contract §3), and it is the only
 *  non-canonical value accepted: anything else is a value this build cannot read
 *  back, and turning it into a clear would be a silent data change. */
function normalizeGroupBy(value: unknown): Field | null {
  if (typeof value !== "string" || value.length > QUERY_DISPLAY_MAX_FIELD) return null;
  if (value === "") return "";
  return canonicalGroupField(value);
}

/** The one normalizer. `null` means "this is not a draft this build can carry".
 *
 *  It is all-or-nothing by design. A partially applied draft would show the user
 *  a display they never chose, and a truncated list is indistinguishable from a
 *  deliberate short one, so a single bad entry refuses the whole object and the
 *  caller falls back to its own "no draft" behaviour.
 *
 *  The returned object is always FRESH, including every list and tuple, so a
 *  caller that keeps mutating the array it passed in cannot reach back into a
 *  route or a history snapshot that has already been recorded. */
export function normalizeQueryDisplayDraft(value: unknown): QueryDisplayDraft | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const o = value as Record<string, unknown>;
  const draft: QueryDisplayDraft = {};
  for (const key of DRAFT_KEYS) {
    const raw = o[key];
    if (raw === undefined) continue;
    if (key === "sort") {
      const sort = normalizeSort(raw);
      if (!sort) return null;
      draft.sort = sort;
    } else if (key === "columns") {
      const columns = normalizeColumns(raw);
      if (!columns) return null;
      draft.columns = columns;
    } else if (key === "aggregates") {
      const aggregates = normalizeAggregates(raw);
      if (!aggregates) return null;
      draft.aggregates = aggregates;
    } else if (key === "group_by") {
      const group = normalizeGroupBy(raw);
      if (group === null) return null;
      draft.group_by = group;
    } else {
      if (typeof raw !== "number" || !Number.isSafeInteger(raw)
        || raw < 0 || raw > QUERY_DISPLAY_MAX_SAMPLE) return null;
      draft.sample = raw;
    }
  }
  // The size bound is asked of the RECOGNIZED data only: the route's own id,
  // source and presentation carry their own existing bounds, and an unknown key
  // has already been dropped by the time we get here.
  if (JSON.stringify(draft).length > QUERY_DISPLAY_MAX_JSON) return null;
  return draft;
}

/** The display a workspace should actually render, without parsing anything.
 *
 *  Three inputs, three distinct jobs:
 *
 *   * `presentation` is the view, always. It is the route's, so a draft can
 *     never disagree with the tab the user is looking at.
 *   * an ABSENT draft inherits every non-view setting the query text already
 *     stated — the workspace shows what the query says until someone changes it.
 *   * a PRESENT draft replaces that half wholesale, so `{}` is the way to say
 *     "clear all of it" and is not the same as saying nothing.
 *
 *  Lists are copied out of both inputs, so the caller owns what it gets back and
 *  cannot write through it into a route or a parse result. */
export function queryDisplaySettings(
  draft: QueryDisplayDraft | undefined,
  parsed: ViewSettings | undefined,
  presentation: ViewKind,
): ViewSettings {
  const source: QueryDisplayDraft = draft ?? {
    ...(parsed?.sort !== undefined ? { sort: parsed.sort } : {}),
    ...(parsed?.group_by !== undefined ? { group_by: parsed.group_by } : {}),
    ...(parsed?.columns !== undefined ? { columns: parsed.columns } : {}),
    ...(parsed?.aggregates !== undefined ? { aggregates: parsed.aggregates } : {}),
    ...(parsed?.sample !== undefined ? { sample: parsed.sample } : {}),
  };
  return {
    view: presentation,
    ...(source.sort !== undefined ? { sort: source.sort.map(([f, d]): [Field, SortDir] => [f, d]) } : {}),
    ...(source.group_by !== undefined ? { group_by: source.group_by } : {}),
    ...(source.columns !== undefined ? { columns: [...source.columns] } : {}),
    ...(source.aggregates !== undefined
      ? { aggregates: source.aggregates.map(([f, fn]): [Field, AggFn] => [f, fn]) }
      : {}),
    ...(source.sample !== undefined ? { sample: source.sample } : {}),
  };
}

/** The result kind a display resolution is being asked about. */
export type QueryResultKind = "page" | "block";

/** The state one surface holds about a query's display, singular and scoped.
 *
 *  A workspace fills it from its route; an inline macro fills it from the
 *  flattened `ParsedQuery` fields Rust returns. Presence is stated by
 *  `Object.hasOwn` on the caller's side and carried here as an explicit
 *  `Partial`, never by truthiness: `{}`, `[]`, `group_by: ""` and `sample: 0`
 *  are all things a user can mean. */
export interface QueryScopedDisplayState {
  /** The singular presentation — the compatibility answer both kinds fall back
   *  to when their own is unset. */
  presentation: ViewKind;
  /** The singular non-view settings: a route's own draft, or the parse's view
   *  where the surface has no draft of its own. */
  display?: QueryDisplayDraft;
  /** The query text's own settings, which an ABSENT singular draft inherits. */
  parsed?: ViewSettings;
  pagePresentation?: ViewKind;
  blockPresentation?: ViewKind;
  /** Present with `undefined` means "a scoped draft exists and is empty" is NOT
   *  what this says — use `Object.hasOwn`. Callers build this object with the
   *  key omitted entirely when there is no scoped draft. */
  pageDisplay?: QueryDisplayDraft;
  blockDisplay?: QueryDisplayDraft;
}

/** **The one resolver for a mixed result's two halves** (I-12).
 *
 *  Three rules, and each of them is a distinct user statement:
 *
 *   * A scoped PRESENTATION independently overrides the singular one. Switching
 *     one section's view says nothing about the other's.
 *   * An ABSENT scoped draft inherits the entire singular non-view snapshot —
 *     which is itself the singular draft, or the query text where there is no
 *     draft.
 *   * A PRESENT scoped draft replaces that snapshot WHOLESALE. `{}` is
 *     therefore the clear, and a present draft's missing members are NOT merged
 *     back from the singular state: a scoped draft says what this section
 *     shows, in full.
 *
 *  Presence is asked with `Object.hasOwn`, never with `??` or truthiness. An
 *  empty column list, a cleared grouping and a zero sample are all values a
 *  user chose, and every one of them is falsy. */
export function queryScopedDisplaySettings(
  state: QueryScopedDisplayState,
  kind: QueryResultKind,
): ViewSettings {
  const page = kind === "page";
  const scopedKey = page ? "pageDisplay" : "blockDisplay";
  const presentation = (page ? state.pagePresentation : state.blockPresentation)
    ?? state.presentation;
  if (Object.hasOwn(state, scopedKey)) {
    // Present: this scope's own complete snapshot. `parsed` is deliberately not
    // passed on — a present draft does not inherit from the query text either.
    return queryDisplaySettings(state[scopedKey] ?? {}, undefined, presentation);
  }
  return queryDisplaySettings(state.display, state.parsed, presentation);
}

/** Resolve one half of a mixed-result route. Scoped presentation and draft
 *  override the singular compatibility fields independently. A present `{}`
 *  is therefore a clear, while an absent scoped draft inherits `display`. */
export function queryResultDisplaySettings(
  route: QueryRoute,
  parsed: ViewSettings | undefined,
  target: QueryResultKind,
): ViewSettings {
  // The route IS the state; the workspace keeps its existing caller shape and
  // the resolution above is the only place the three rules are written down.
  const state: QueryScopedDisplayState = {
    presentation: route.presentation,
    ...(Object.hasOwn(route, "display") ? { display: route.display } : {}),
    ...(parsed !== undefined ? { parsed } : {}),
    ...(route.pagePresentation !== undefined ? { pagePresentation: route.pagePresentation } : {}),
    ...(route.blockPresentation !== undefined ? { blockPresentation: route.blockPresentation } : {}),
    ...(Object.hasOwn(route, "pageDisplay") ? { pageDisplay: route.pageDisplay } : {}),
    ...(Object.hasOwn(route, "blockDisplay") ? { blockDisplay: route.blockDisplay } : {}),
  };
  return queryScopedDisplaySettings(state, target);
}

/** The same resolution for an INLINE query, whose scoped state arrives already
 *  flattened beside `{query, view}` by `query_parse` (SPEC §7.6).
 *
 *  Rust states presence the same way this module does — a missing marker leaves
 *  the field absent, and a marker with no members returns an empty object — so
 *  the adaptation is `Object.hasOwn` on the parse, not a second reading of the
 *  property bytes. `view` is the singular merge Rust already performed, so it
 *  plays the singular draft's part here. */
export function queryParsedDisplaySettings(
  parsed: {
    view?: ViewSettings;
    page_presentation?: ViewKind;
    block_presentation?: ViewKind;
    page_display?: QueryDisplayDraft;
    block_display?: QueryDisplayDraft;
  } | undefined,
  kind: QueryResultKind,
): ViewSettings {
  const view = parsed?.view ?? {};
  const state: QueryScopedDisplayState = {
    presentation: view.view ?? "list",
    display: {
      ...(view.sort !== undefined ? { sort: view.sort } : {}),
      ...(view.group_by !== undefined ? { group_by: view.group_by } : {}),
      ...(view.columns !== undefined ? { columns: view.columns } : {}),
      ...(view.aggregates !== undefined ? { aggregates: view.aggregates } : {}),
      ...(view.sample !== undefined ? { sample: view.sample } : {}),
    },
    ...(parsed?.page_presentation !== undefined
      ? { pagePresentation: parsed.page_presentation } : {}),
    ...(parsed?.block_presentation !== undefined
      ? { blockPresentation: parsed.block_presentation } : {}),
    ...(parsed && Object.hasOwn(parsed, "page_display")
      ? { pageDisplay: parsed.page_display } : {}),
    ...(parsed && Object.hasOwn(parsed, "block_display")
      ? { blockDisplay: parsed.block_display } : {}),
  };
  return queryScopedDisplaySettings(state, kind);
}

/** The complete effective non-view snapshot a FIRST scoped edit clones.
 *
 *  §7 of the design: a scoped draft is complete or it is absent, so the first
 *  edit in a namespace has to write down everything that namespace was already
 *  showing — otherwise the edit would silently clear the facts it did not
 *  mention. `view` is dropped because presentation is independently scoped. */
export function queryScopedDraftFrom(settings: ViewSettings): QueryDisplayDraft {
  return {
    ...(settings.sort !== undefined ? { sort: settings.sort.map(([f, d]): [Field, SortDir] => [f, d]) } : {}),
    ...(settings.group_by !== undefined ? { group_by: settings.group_by } : {}),
    ...(settings.columns !== undefined ? { columns: [...settings.columns] } : {}),
    ...(settings.aggregates !== undefined
      ? { aggregates: settings.aggregates.map(([f, fn]): [Field, AggFn] => [f, fn]) }
      : {}),
    ...(settings.sample !== undefined ? { sample: settings.sample } : {}),
  };
}
