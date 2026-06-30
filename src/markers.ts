// Single source of truth for the task-marker set and its subsets. Before this, the
// list was hand-copied into ~6 frontend spots (render chip, marker cycler, priority
// anchor, query builder, "carry unfinished tasks", the mock) and they had drifted —
// e.g. the carry open-task set + the query-builder set were both missing IN-PROGRESS
// and WAIT, so carry silently skipped those tasks and the builder couldn't filter
// them. Everything now imports from here.
//
// Backend mirror: `crates/tine-core/src/doc.rs` `MARKERS` (cross-language, so it
// can't share this literal — `markers.test.ts` guards the two sets against drift).
//
// Order is prefix-safe for `^(…)\b` alternation / `startsWith(m+" ")` scans: the
// longer of any prefix pair comes first (WAITING before WAIT).
export const MARKERS = [
  "TODO",
  "DOING",
  "NOW",
  "LATER",
  "WAITING",
  "WAIT",
  "STARTED",
  "IN-PROGRESS",
  "DONE",
  "CANCELED",
  "CANCELLED",
] as const;

/** "Closed" markers — a task in one of these is finished/dropped (not an open task,
 *  not carried forward). `done` chip styling keys off this too. */
export const DONE_MARKERS: ReadonlySet<string> = new Set(["DONE", "CANCELED", "CANCELLED"]);

/** "Open" (unfinished) task markers = all markers minus the closed ones. Used by
 *  "carry unfinished tasks" and any open-task scan. */
export const OPEN_MARKERS: ReadonlySet<string> = new Set(
  MARKERS.filter((m) => !DONE_MARKERS.has(m))
);

/** A leading task marker at the very start of a line, as a whole word. */
export const MARKER_RE = new RegExp(`^(${MARKERS.join("|")})\\b`);
