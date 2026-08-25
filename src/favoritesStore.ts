// Wiring between the Favorites arrangement page and the app.
//
// Two stores, one direction each, so there is never a question of which wins:
//
//   arrangement page  --project-->  config.edn :favorites   (membership, flat)
//   config.edn        --reconcile-> arrangement page        (external changes)
//
// `favorites()` in ui.ts stays the flat membership list every existing consumer
// already reads; this module keeps it and the arrangement in step. The
// arrangement page is created lazily — a graph that never groups anything never
// grows a page it did not ask for.
import { backend } from "./backend";
import type { PageDto } from "./types";
import {
  DEFAULT_FAVORITES_PAGE,
  FAVORITES_PAGE_PROPERTY,
  type FavLayout,
  emptyLayout,
  layoutFromBlocks,
  layoutMembers,
  layoutToMarkdown,
  reconcileLayout,
  uniqueGroupName,
} from "./favoritesLayout";
import { createSignal } from "solid-js";

const [layout, setLayoutSignal] = createSignal<FavLayout>(emptyLayout());
export const favoritesLayout = layout;

/** The page that owns the arrangement, once this graph has one. */
const [layoutPage, setLayoutPage] = createSignal<string | null>(null);
export const favoritesLayoutPage = layoutPage;

/** Revision the arrangement page was loaded at, for the save baseline. */
let layoutRev: string | null = null;

export function resetFavoritesLayout() {
  setLayoutSignal(emptyLayout());
  setLayoutPage(null);
  layoutRev = null;
}

/** Adopt the arrangement for a freshly opened graph. `membership` is
 *  config.edn's `:favorites` — the authority on WHICH pages are favorited, so
 *  a change made in Logseq while Tine was closed is honoured here. */
export async function loadFavoritesLayout(
  membership: string[],
  page: string | null | undefined,
): Promise<FavLayout> {
  resetFavoritesLayout();
  if (page) {
    setLayoutPage(page);
    try {
      const dto = await backend().getPage(page, "page");
      if (dto) {
        layoutRev = dto.rev ?? null;
        setLayoutSignal(reconcileLayout(layoutFromBlocks(dto.blocks), membership));
        return layout();
      }
    } catch {
      // A missing or unreadable arrangement page must never cost the user their
      // favorites: config.edn still has membership, so fall back to a flat list.
    }
  }
  setLayoutSignal(reconcileLayout(emptyLayout(), membership));
  return layout();
}

/** Fold an externally-changed membership list into the arrangement WITHOUT
 *  writing anything back — used when config.edn changed under us. */
export function adoptExternalMembership(membership: string[]): FavLayout {
  const next = reconcileLayout(layout(), membership);
  setLayoutSignal(next);
  return next;
}

function layoutPageDto(name: string, next: FavLayout): PageDto {
  const markdown = layoutToMarkdown(next);
  const blocks = markdown
    .split("\n")
    .filter((line) => line.trim() !== "")
    .reduce<{ raw: string; indented: boolean }[]>((acc, line) => {
      const indented = line.startsWith("\t");
      acc.push({ raw: line.replace(/^\t?- /, ""), indented });
      return acc;
    }, [])
    .reduce<PageDto["blocks"]>((roots, entry) => {
      const block = { id: "", raw: entry.raw, collapsed: false, children: [] };
      if (entry.indented && roots.length) roots[roots.length - 1].children.push(block);
      else roots.push(block);
      return roots;
    }, []);
  return {
    name,
    kind: "page",
    title: name,
    pre_block: `${FAVORITES_PAGE_PROPERTY}:: true`,
    blocks,
  };
}

/** Persist an arrangement: write the page, then project membership into
 *  config.edn. The page is created (and recorded in config.edn) on first use.
 *
 *  Order matters. The page is written FIRST because it is the richer artifact;
 *  if the projection then fails, config.edn still holds the previous membership
 *  and the next reconcile folds the page back into agreement. The reverse order
 *  could leave membership naming pages the arrangement has never heard of. */
export async function persistFavoritesLayout(next: FavLayout): Promise<void> {
  setLayoutSignal(next);
  const names = layoutMembers(next).map((item) => item.name);
  let page = layoutPage();
  const carriesArrangement = next.some((group) => group.name !== null);
  if (!page && !carriesArrangement) {
    // Nothing here that config.edn cannot express. A user who never groups
    // anything never grows a page in their graph they did not ask for, and
    // their favorites behave exactly as they did before this existed.
    await backend().setFavorites(names).catch(() => {});
    return;
  }
  if (!page) {
    page = DEFAULT_FAVORITES_PAGE;
    setLayoutPage(page);
  }
  try {
    const saved = await backend().savePage(layoutPageDto(page, next), layoutRev);
    layoutRev = typeof saved === "string" ? saved : saved.revision;
    await backend().setFavoritesPage(page);
  } catch {
    // Losing the arrangement is survivable; losing membership is not. Fall
    // through and still project, so the favorites themselves persist.
  }
  await backend().setFavorites(names).catch(() => {});
}

// ---------------------------------------------------------------------------
// Arrangement mutations.
//
// The sidebar renders one flat run of rows across all groups (group headers are
// not rows), so a drag speaks in GLOBAL row indices plus the group the drop
// landed in. That keeps `rowReorder` a single-list helper — the thing it
// already is and is already tested for — instead of teaching it about
// containers, while still allowing a drop across a group boundary, which is
// otherwise ambiguous at the boundary index.

/** (group index, index within group) for a global row index. */
export function locateRow(next: FavLayout, global: number): { group: number; index: number } | null {
  let seen = 0;
  for (let g = 0; g < next.length; g += 1) {
    const size = next[g].items.length;
    if (global < seen + size) return { group: g, index: global - seen };
    seen += size;
  }
  return null;
}

/** Move one favorite to a global row position, landing it in `targetGroup`. */
export function moveFavoriteRow(
  next: FavLayout,
  from: number,
  to: number,
  targetGroup: number,
): FavLayout {
  const source = locateRow(next, from);
  if (!source) return next;
  const groups = next.map((group) => ({ ...group, items: [...group.items] }));
  const [item] = groups[source.group].items.splice(source.index, 1);
  if (!item) return next;
  const clampedGroup = Math.max(0, Math.min(targetGroup, groups.length - 1));
  // `to` is a global index in the array AFTER removal, so resolve it there.
  const landing = locateRow(
    groups.map((group) => ({ ...group })),
    Math.max(0, to),
  );
  const index =
    landing && landing.group === clampedGroup
      ? landing.index
      : groups[clampedGroup].items.length;
  groups[clampedGroup].items.splice(index, 0, item);
  return groups;
}

export function addGroup(next: FavLayout, desired = "New group"): FavLayout {
  return [...next, { name: uniqueGroupName(next, desired), items: [], passthrough: [] }];
}

export function renameGroup(next: FavLayout, groupIndex: number, name: string): FavLayout {
  const trimmed = name.trim();
  if (!trimmed || groupIndex <= 0 || groupIndex >= next.length) return next;
  return next.map((group, i) =>
    i === groupIndex ? { ...group, name: uniqueGroupName(next.filter((_, j) => j !== i), trimmed) } : group
  );
}

/** Delete a group WITHOUT unfavoriting anything: its members move to the
 *  ungrouped section. Capacities states this contract explicitly and it is the
 *  one users rely on; Obsidian's bookmark folders lose the reference instead. */
export function deleteGroup(next: FavLayout, groupIndex: number): FavLayout {
  if (groupIndex <= 0 || groupIndex >= next.length) return next;
  const doomed = next[groupIndex];
  return next
    .map((group, i) =>
      i === 0
        ? { ...group, items: [...group.items, ...doomed.items], passthrough: [...group.passthrough, ...doomed.passthrough] }
        : group
    )
    .filter((_, i) => i !== groupIndex);
}

export function setGroupCollapsed(next: FavLayout, groupIndex: number, collapsed: boolean): FavLayout {
  if (groupIndex <= 0 || groupIndex >= next.length) return next;
  return next.map((group, i) => (i === groupIndex ? { ...group, collapsed: collapsed || undefined } : group));
}
