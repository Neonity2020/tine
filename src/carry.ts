// "Carry unfinished tasks to today" (feature B). The store engine
// (carryUnfinished) does the tree surgery; this orchestrates loading the days it
// needs into the working set, then surfaces the result. Days are passed
// newest→oldest so the newest carried tasks end up on top of today.
//
// Carry is an N-source cross-page move, so it goes through the ONE front door
// (`src/crossPageMove.ts`) like every other shape: the door reads admission,
// pre-flushes the source days while they still hold the tasks, re-checks the
// plan across that await, and owns the source barrier, the destination-first
// order and the durable recovery record.
//
// This file used to keep its own copy of that choreography, and the copy was
// missing the barrier (K6 census H1): a conflicted today toasted "your moved
// tasks are kept in the editor", and then any later edit to a source day let the
// ordinary debounced save write the post-removal file — carrying the tasks out
// of the only file that still had them. Nothing held those sources, because only
// the other four shapes went through `persistCrossPage`.

import { backend } from "./backend";
import { carryUnfinished, ensurePageLoaded, pageByName } from "./store";
import { journalTitle } from "./journal";
import { requestCrossPageMove } from "./crossPageMove";
import { graphBinding } from "./persistence";
import { carryHeaderText, carryKeepsContext, pushToast } from "./ui";
import { openJournals } from "./router";
import type { PageDto } from "./types";

async function ensureLoaded(name: string, kind: "journal" | "page"): Promise<boolean> {
  if (pageByName(name)) return true;
  const binding = graphBinding();
  const dto = await backend().getPage(name, kind);
  if (dto) {
    // A refusal here used to be invisible: this returned `true` unconditionally
    // after calling `ensurePageLoaded`, so carry went on to move blocks into
    // whichever editor happened to be loaded under that name — the wrong file.
    // Carry must stop instead. (GH #254 increment 3.)
    if (await ensurePageLoaded(dto, { expectedGraphBinding: binding })) return false;
    return true;
  }
  return false;
}

/** Make sure today's journal is in the working set (synthesize an empty one if
 *  it has no file yet, like the feed does). */
async function ensureToday(): Promise<string | null> {
  const t = journalTitle(new Date());
  if (!pageByName(t)) {
    const binding = graphBinding();
    const dto = await backend().getPage(t, "journal");
    const page: PageDto =
      dto ?? { name: t, kind: "journal", title: t, pre_block: null, blocks: [{ id: `new-${t}`, raw: "", collapsed: false, children: [] }] };
    // Previously returned the title unconditionally, so a refused today made
    // carry proceed against an editor it had not loaded. Null means stop.
    if (await ensurePageLoaded(page, { expectedGraphBinding: binding })) return null;
  }
  return t;
}

/**
 * The one carry choreography: state the intent, do the tree surgery when the
 * door says the plan still holds, then report on durability.
 *
 * Carry is the one shape that AWAITS durability (`outcome.landed`) rather than
 * firing and forgetting: it reloads the journals feed on success, and a reload
 * re-reads the files, so reloading before the write landed would drop the
 * carried blocks out of memory.
 */
async function runCarry(today: string, days: readonly string[], blockedToast: string): Promise<void> {
  let moved = 0;
  let surgeryRan = false;
  const outcome = await requestCrossPageMove<string[]>({
    operation: "carry",
    blockedToast,
    plan: () => {
      const live = [...new Set(days)].filter((day) => day !== today && pageByName(day));
      return live.length ? live : null;
    },
    intent: (live) => ({ sourcePages: live, destinationPage: today, roots: [] }),
    apply: (live) => {
      surgeryRan = true;
      moved = carryUnfinished(live, carryKeepsContext(), carryHeaderText());
      // "Nothing to carry" is a successful no-op, not a move: it must not open a
      // recovery record or write a file.
      return moved > 0;
    },
  });

  if (!outcome.applied) {
    if (surgeryRan && moved === 0) pushToast("No unfinished tasks to carry");
    return;
  }
  // If a touched page couldn't be saved (conflict / disk error), DON'T reload the
  // journals feed — that would re-read the old files and drop the carried blocks
  // from memory. Leave the move in memory and surface the failure.
  if (!(await outcome.landed)) {
    pushToast("Carry couldn't be saved — resolve the conflict; your moved tasks are kept in the editor.", "error");
    return;
  }
  // TODO(S2): explicit pane handle for the journals feed pane.
  openJournals({ inPlace: true }); // a carry reloads the feed in place, not a new tab
  pushToast(`Carried ${moved} item${moved === 1 ? "" : "s"} to today`);
}

/** Carry unfinished tasks from the previous *non-empty* day to today. "Previous
 *  day" means the most recent journal before today that actually has content
 *  (not literally yesterday, which is often blank). */
export async function carryPrevDay(): Promise<void> {
  const today = new Date();
  const todayKey =
    today.getFullYear() * 10000 + (today.getMonth() + 1) * 100 + today.getDate();
  let days: number[] = [];
  try {
    days = await backend().journalContentDays();
  } catch {
    days = [];
  }
  const prevKey = days.filter((k) => k < todayKey).sort((a, b) => a - b).pop();
  if (prevKey == null) {
    pushToast("No previous day with content to carry from");
    return;
  }
  const d = new Date(Math.floor(prevKey / 10000), (Math.floor(prevKey / 100) % 100) - 1, prevKey % 100);
  await carryDay(journalTitle(d));
}

/** Carry one day's unfinished tasks to today (used from a day's context menu). */
export async function carryDay(pageName: string): Promise<void> {
  const today = await ensureToday();
  if (!today) return;
  if (pageName === today) return;
  if (!(await ensureLoaded(pageName, "journal"))) return;
  await runCarry(today, [pageName], "Couldn't carry — that day has unsaved changes to resolve first.");
}

/** Carry unfinished tasks from the last `days` days (today−1 … today−days) to
 *  today, newest first. Only days that have a file are touched. */
export async function carryDaysBack(days: number): Promise<void> {
  const today = await ensureToday();
  if (!today) return;
  const base = new Date();
  const candidates: string[] = [];
  for (let i = 1; i <= days; i++) {
    const d = new Date(base);
    d.setDate(d.getDate() - i);
    candidates.push(journalTitle(d));
  }
  // Load all the day files in parallel rather than one IPC round-trip at a time.
  const loaded = await Promise.all(candidates.map((t) => ensureLoaded(t, "journal")));
  const titles = candidates.filter((_, i) => loaded[i]); // skip days with no file
  await runCarry(today, titles, "Couldn't carry — a day has unsaved changes to resolve first.");
}
