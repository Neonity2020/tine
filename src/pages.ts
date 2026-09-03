import { createMemo, createResource, createRoot } from "solid-js";
import { backend } from "./backend";
import { dataRev, graphEpoch, pageInventoryRev } from "./ui";
import { waitForWarmCache } from "./warmCache";
import type { PageEntry } from "./types";

// ONE graph-wide physical page list and reference-name list, shared by every
// namespace + sidebar consumer. Physical entries keep paths/owners for All Pages;
// reference names only enrich the shared namespace inventory after warm completes.
// The physical list has its own create/delete invalidation lane, so ordinary saves
// never turn into a whole-page-list IPC.
//
// Before this, `NamespaceHierarchy` keyed its `listPages()` resource on the page
// NAME, so it re-pulled the whole page list over IPC (+ an O(allPages) scan) on
// EVERY page navigation, even for a page with no namespace; and the sidebar, the
// namespace tree, and the {{namespace}} macro each ran their own duplicate
// whole-graph fetch. On a large graph that was hundreds of ms + GC churn per nav.
//
// Own root: lives for the app's lifetime by design (mirrors blockRefCounts.ts /
// resolveBatch.ts). The initial epoch (0) is a real resource key, so first paint
// fetches; a later graphEpoch change refetches and fixes the old mount/load race.
/** Test seam for Harvest W4-P1 item 2: fires once per execution of the
 *  page-name merge memo body, BEFORE its O(|pages|) `Set` is built. A counter
 *  placed downstream of the memo instead would report zero even while the Set
 *  was rebuilt on every typing lull. */
export const __pagesTestHooks: { onMergePageNames?: () => void } = {};

const pageInventory = createRoot(() => {
  const [physicalPages] = createResource(
    () => ({ epoch: graphEpoch(), inventory: pageInventoryRev() }),
    async ({ epoch, inventory }) => {
      const pages = await backend().listPages().catch(() => [] as PageEntry[]);
      return epoch === graphEpoch() && inventory === pageInventoryRev() ? pages : [];
    }
  );
  // The last set the backend handed us, with the digest that identified it. This
  // resource re-runs after every typing lull — it is keyed on `dataRev`, which a
  // save bumps — and the answer almost never changes, because typing inside a
  // block rarely adds or removes a `[[link]]`. Presenting the digest lets the
  // backend reply with an integer instead of several thousand strings that would
  // be JSON-parsed on the UI thread. (Direct Files performance audit 2026-08-09,
  // finding F7.) Reset on a graph switch: digests are per-graph.
  let known: { epoch: number; digest: number; names: string[] } | null = null;
  const [referencedNames] = createResource(
    () => ({ epoch: graphEpoch(), revision: dataRev(), inventory: pageInventoryRev() }),
    async ({ epoch, revision, inventory }) => {
      // `referenced_page_names` deliberately returns empty before the Rust warm
      // cache exists. Wait for its completion event instead, so that early empty
      // result cannot get memoized for this frontend revision.
      if (!(await waitForWarmCache(epoch))) return [];
      if (epoch !== graphEpoch() || revision !== dataRev() || inventory !== pageInventoryRev()) return [];
      const carried = known?.epoch === epoch ? known : null;
      const answer = await backend()
        .referencedPageNames(carried?.digest ?? null)
        .catch(() => null);
      if (!answer) return carried?.names ?? [];
      // A null `names` means "unchanged", so it may only be honoured against the
      // set that digest described. Without a carried set there is nothing to
      // reuse, and treating it as empty would erase the inventory.
      const names = answer.names ?? carried?.names ?? [];
      known = { epoch, digest: answer.digest, names };
      return epoch === graphEpoch() && revision === dataRev() && inventory === pageInventoryRev()
        ? names
        : [];
    }
  );
  // Physical display spellings win case-insensitively; the reference-only names
  // then fill gaps. This is the sole complete page-name inventory for namespace
  // consumers, while `allPages` intentionally remains physical-only.
  const names = createMemo(() => {
    __pagesTestHooks.onMergePageNames?.();
    const seen = new Set<string>();
    const inventory: string[] = [];
    const add = (name: string) => {
      const key = name.toLowerCase();
      if (!seen.has(key)) {
        seen.add(key);
        inventory.push(name);
      }
    };
    for (const page of physicalPages() ?? []) add(page.name);
    for (const name of referencedNames() ?? []) add(name);
    return inventory;
  });
  return { physicalPages, names };
});

/** All pages in the current graph (`undefined` until the first load). Reactive:
 *  reading it in a tracking scope re-runs when the list (re)loads. */
export function allPages(): PageEntry[] | undefined {
  return pageInventory.physicalPages();
}

/** Complete physical + reference-only page-name inventory (`[]` until loaded).
 *  Reactive; physical names keep their display spelling on case-folded collisions. */
export function allPageNames(): string[] {
  return pageInventory.names();
}

function parentPathLabel(p: PageEntry): string {
  const root = p.kind === "journal" ? "journals/" : "pages/";
  const rel = p.path.startsWith(root) ? p.path.slice(root.length) : p.path;
  const slash = rel.lastIndexOf("/");
  return slash >= 0 ? `${rel.slice(0, slash)}/` : root;
}

/** Disambiguating labels for a whole page list, in ONE pass over it.
 *
 *  Two pages can share a display name (same title in different folders, or the
 *  same stem as `.md` and `.org`); such a row grows a parent sub-path, and if
 *  that is still ambiguous, its full path. Deciding that per row means scanning
 *  the entire list per row — O(rows × pages), which the sidebar paid on every
 *  render: 21 ms at 5,000 pages, 39 ms at 20,000, for 300 visible rows.
 *
 *  Counting first makes each row a map lookup. (Direct Files performance audit,
 *  2026-08-09, finding F9.) */
export function pageListLabels(pages: PageEntry[]): (p: PageEntry) => string {
  const nameKey = (p: PageEntry) => `${p.kind} ${p.name}`;
  const parentKey = (p: PageEntry) => `${nameKey(p)} ${parentPathLabel(p)}`;
  const byName = new Map<string, number>();
  const byNameAndParent = new Map<string, number>();
  for (const p of pages) {
    byName.set(nameKey(p), (byName.get(nameKey(p)) ?? 0) + 1);
    byNameAndParent.set(parentKey(p), (byNameAndParent.get(parentKey(p)) ?? 0) + 1);
  }
  // Keyed by VALUE, not identity: a caller may hold a copy of an entry rather
  // than the array element itself, and that must not silently change its label.
  return (p) => {
    if ((byName.get(nameKey(p)) ?? 0) < 2) return p.name;
    const parent = parentPathLabel(p);
    const unique = (byNameAndParent.get(parentKey(p)) ?? 0) === 1;
    return `${p.name} — ${unique ? parent : p.path}`;
  };
}

/** Single-row form, for callers that hold no precomputed index. Same answer as
 *  `pageListLabels`; a list-rendering caller should build the index once. */
export function pageListLabel(p: PageEntry, pages: PageEntry[]): string {
  return pageListLabels(pages)(p);
}
