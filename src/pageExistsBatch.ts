import { createSignal } from "solid-js";
import { backend } from "./backend";
import { graphEpoch, pageInventoryRev } from "./ui";

// Does the page behind a `[[ref]]` exist? Used to dim links that will open a
// blank page (Martin's 2026-08-09 tine-choco report: a `title::`/filename
// mismatch dead-ended 29 links with no visible signal).
//
// Batched exactly like `pageIconBatch`: every reference rendered in one
// microtask tick becomes ONE `existing_page_names` IPC, and each name is asked
// at most once per (graph × page-inventory) state. The backend answer costs one
// memoized page-list read, so per-reference cost here is a signal read plus an
// object lookup.
//
// UNKNOWN MEANS ALIVE. A reference renders normally until its batch resolves,
// so a page of live links never flashes as dead; only a genuinely missing
// target changes appearance, once.
//
// Answers live until the graph changes OR its page inventory moves (GH #355):
// a same-session page creation must restyle every [[ref]] to that page
// immediately, and a deletion must not leave a stale positive result behind.
let cacheRev = -1;
let cacheInventoryRev = -1;
const [known, setKnown] = createSignal<Record<string, boolean>>({});
const requested = new Set<string>();
let pending: string[] = [];
let scheduled = false;

function ensureRev() {
  const epoch = graphEpoch();
  const inventory = pageInventoryRev();
  if (epoch !== cacheRev || inventory !== cacheInventoryRev) {
    cacheRev = epoch;
    cacheInventoryRev = inventory;
    setKnown({});
    requested.clear();
    pending = [];
  }
}

function flush() {
  scheduled = false;
  if (!pending.length) return;
  const batch = pending;
  pending = [];
  const batchRev = cacheRev;
  const batchInventoryRev = cacheInventoryRev;
  void backend()
    .existingPageNames(batch)
    .then((existing) => {
      if (graphEpoch() !== batchRev || pageInventoryRev() !== batchInventoryRev) return;
      const alive = new Set(existing);
      // Only re-render when the batch actually found a MISSING page. A graph
      // whose links all resolve — the overwhelming majority — leaves the signal
      // untouched and costs zero re-renders, mirroring pageIconBatch.
      if (!batch.some((name) => !alive.has(name))) return;
      setKnown((previous) => {
        const next = { ...previous };
        for (const name of batch) next[name] = alive.has(name);
        return next;
      });
    })
    .catch(() => {});
}

/** Reactive: `true` when the page is known NOT to exist. Unknown → `false`. */
export function pageIsMissing(name: string): boolean {
  ensureRev();
  if (!requested.has(name)) {
    requested.add(name);
    pending.push(name);
    if (!scheduled) {
      scheduled = true;
      queueMicrotask(flush);
    }
  }
  return known()[name] === false;
}

/** Test seam: forget every answer, as a graph switch would. */
export function resetPageExistsBatch() {
  cacheRev = -1;
  cacheInventoryRev = -1;
  setKnown({});
  requested.clear();
  pending = [];
  scheduled = false;
}
