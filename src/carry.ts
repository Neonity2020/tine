// "Carry unfinished tasks to today" (feature B). The store engine
// (carryUnfinished) does the tree surgery; this orchestrates loading the days it
// needs into the working set, then surfaces the result. Days are passed
// newest→oldest so the newest carried tasks end up on top of today.

import { backend } from "./backend";
import {
  pageByName,
  ensurePageLoaded,
  carryUnfinished,
  flushPage,
  isDirty,
  markDirty,
  prepareCrossPageSources,
  withDirectMoveRecord,
} from "./store";
import { journalTitle } from "./journal";
import { dispatchCarry, MANAGED_MULTI_SOURCE_MOVE_UNAVAILABLE_TOAST } from "./storageDispatch";
import { graphBinding } from "./persistence";
import { carryKeepsContext, carryHeaderText, pushToast } from "./ui";
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

// Persist the touched pages to disk NOW, before any feed reload — otherwise
// navigating to journals reloads the (still-old) files and clobbers the move.
// Returns whether every dirty touched page actually saved.
// Persist `today` (the ADDITION side) FIRST and only flush the source days once it
// lands — so a today-conflict can't leave the carried blocks removed from their
// source files but never written to today (a removal-only, data-losing state).
async function persist(today: string, sources: string[]): Promise<boolean> {
  // Carry is an N-source cross-page move, so it runs inside the same durable
  // recovery record every other Direct cross-page move uses (I-3/I-2): composed
  // before today's journal is written, retired once every day file is durably
  // terminal, and completed or rolled back by the next open if the process dies
  // in between. See `docs/contracts/direct-move-recovery.md`.
  return withDirectMoveRecord(today, sources, async () => {
    // Destination (today) must land first. carryUnfinished intentionally left the
    // source days NOT dirty, so nothing can save a source removal until today is
    // safely written — only THEN do we mark + flush the sources.
    if (isDirty(today) && !(await flushPage(today))) return false;
    const uniq = [...new Set(sources)].filter((n) => n !== today);
    for (const n of uniq) markDirty(n);
    const results = await Promise.all(uniq.map((n) => flushPage(n)));
    return results.every(Boolean);
  });
}

/** Managed storage has no carry arm.
 *
 *  Carry gathers unfinished tasks from N journal days into today, and the native
 *  managed move accepts exactly ONE source page — the same limit the multi-source
 *  relative move already refuses under. Until B1 this refusal did not exist:
 *  carry ran the Direct choreography under every admission, writing journal files
 *  directly beneath a managed binding. The refusal is taken at the operation
 *  boundary, before `carryUnfinished` touches memory, so the editor is never left
 *  holding a mutation storage never accepted.
 *
 *  Lifting the multi-source limit is an undecided product question (spec B), not
 *  an implementation gap — so this refuses rather than guessing at a managed
 *  carry. */
function refuseManagedCarry(): void {
  pushToast(MANAGED_MULTI_SOURCE_MOVE_UNAVAILABLE_TOAST, "error");
}

async function report(n: number, today: string, sources: string[]): Promise<void> {
  // If a touched page couldn't be saved (conflict / disk error), DON'T reload the
  // journals feed — that would re-read the old files and drop the carried blocks
  // from memory. Leave the move in memory and surface the failure.
  const saved = await persist(today, sources);
  if (!saved) {
    pushToast("Carry couldn't be saved — resolve the conflict; your moved tasks are kept in the editor.", "error");
    return;
  }
  // TODO(S2): explicit pane handle for the journals feed pane.
  openJournals({ inPlace: true }); // a carry reloads the feed in place, not a new tab
  pushToast(n ? `Carried ${n} item${n === 1 ? "" : "s"} to today` : "No unfinished tasks to carry");
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
  await dispatchCarry<void>(
    { destinationPage: today, sourcePages: [pageName] },
    {
      managed: () => refuseManagedCarry(),
      unavailable: () => {}, // the front door already raised the shared refusal
      direct: async () => {
        // Flush the source day (while it still holds the tasks) before the in-memory
        // move, so a save already pending for it can't write the removal before today
        // is saved. Abort if it can't be flushed (unresolved conflict).
        if (!(await prepareCrossPageSources([pageName]))) {
          pushToast("Couldn't carry — that day has unsaved changes to resolve first.", "error");
          return;
        }
        const n = carryUnfinished([pageName], carryKeepsContext(), carryHeaderText());
        await report(n, today, [pageName]);
      },
    },
  );
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
  await dispatchCarry<void>(
    { destinationPage: today, sourcePages: titles },
    {
      managed: () => refuseManagedCarry(),
      unavailable: () => {},
      direct: async () => {
        // Flush source days (with their tasks intact) before the in-memory move — see carryDay.
        if (!(await prepareCrossPageSources(titles))) {
          pushToast("Couldn't carry — a day has unsaved changes to resolve first.", "error");
          return;
        }
        const n = carryUnfinished(titles, carryKeepsContext(), carryHeaderText());
        await report(n, today, titles);
      },
    },
  );
}
