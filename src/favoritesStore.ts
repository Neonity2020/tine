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
  type FavNode,
  emptyLayout,
  labelNode,
  layoutFromBlocks,
  layoutMembers,
  moveNode,
  nodeAt,
  promoteChildrenAt,
  reconcileLayout,
  uniqueGroupName,
  updateAt,
} from "./favoritesLayout";
import { createSignal } from "solid-js";

const [layout, setLayoutSignal] = createSignal<FavLayout>(emptyLayout());
/** The STORED arrangement: which groups exist, their order and collapse, and
 *  which favorite sits in which. It is not the render source — `favoritesLayout`
 *  in ui.ts reconciles this against live membership on every read, so the two
 *  can never drift out of agreement. */
export const storedFavoritesLayout = layout;

/** The page that owns the arrangement, once this graph has one. */
const [layoutPage, setLayoutPage] = createSignal<string | null>(null);
export const favoritesLayoutPage = layoutPage;

/** Revision the arrangement page was loaded at, for the save baseline. */
let layoutRev: string | null = null;

// Where projected membership goes. A callback rather than an import so this
// module stays free of any dependency on ui.ts, which imports it.
let membershipSink: ((names: string[]) => void) | null = null;
export function setMembershipSink(sink: (names: string[]) => void) {
  membershipSink = sink;
}

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

/** Membership changed inside Tine (a star toggled, a page deleted). Fold it
 *  into the arrangement and persist — but only touch the arrangement page if
 *  this graph already has one. A flat graph stays flat. */
export function membershipChanged(membership: string[]): void {
  const next = reconcileLayout(layout(), membership);
  setLayoutSignal(next);
  // Only touch the arrangement page if this graph already has one; a flat
  // graph stays flat and behaves exactly as it did before groups existed.
  if (layoutPage()) void persistFavoritesLayout(next);
}

/** Fold an externally-changed membership list into the arrangement WITHOUT
 *  writing anything back — used when config.edn changed under us. */
export function adoptExternalMembership(membership: string[]): FavLayout {
  const next = reconcileLayout(layout(), membership);
  setLayoutSignal(next);
  return next;
}

/** The arrangement page's content changed — edited in Tine's own editor, or
 *  delivered from outside. Re-read it, because the page IS the arrangement;
 *  before this, a hand edit was invisible until the graph was reopened.
 *
 *  A page edit is also a membership statement: deleting a `[[link]]` bullet
 *  means "not a favorite any more", and adding one means the opposite. So the
 *  reloaded tree is projected into `:favorites` exactly as a drag would be. It
 *  does NOT write the page back — that would be an echo of the user's own
 *  keystrokes — which is why this projects through the membership sink and
 *  `setFavorites` rather than through `persistFavoritesLayout`. */
export async function favoritesPageChanged(names: readonly string[]): Promise<void> {
  const page = layoutPage();
  if (!page || !names.includes(page)) return;
  let dto: PageDto | null = null;
  try {
    dto = await backend().getPage(page, "page");
  } catch {
    return; // an unreadable page must never cost the user their favorites
  }
  if (!dto) return;
  const rev = dto.rev ?? null;
  // Tine's own write, echoed back by the watcher. Nothing to adopt.
  if (rev !== null && rev === layoutRev) return;
  layoutRev = rev;
  const next = layoutFromBlocks(dto.blocks);
  setLayoutSignal(next);
  const members = layoutMembers(next).map((item) => item.name);
  membershipSink?.(members);
  await backend().setFavorites(members).catch(() => {});
}

function layoutPageDto(name: string, next: FavLayout): PageDto {
  // The arrangement IS a block tree, so it maps straight onto the page's blocks
  // — no Markdown round-trip in the middle. `layoutToMarkdown` remains the
  // human-readable rendering (and what the contract test pins), not the wire
  // format for a save.
  const toBlocks = (nodes: FavNode[]): PageDto["blocks"] =>
    nodes.map((node) => ({
      id: "",
      raw: node.raw,
      collapsed: node.collapsed ?? false,
      children: toBlocks(node.children),
    }));
  const blocks = toBlocks(next);
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
  // The flat membership list follows the arrangement's display order, so a
  // reorder or a move between groups is immediately visible everywhere that
  // reads favorites — not only in the sidebar.
  membershipSink?.(names);
  let page = layoutPage();
  // Anything config.edn's flat `:favorites` list cannot express: a label, or a
  // row nested under another. A graph with neither stays flat and never grows
  // an arrangement page it did not ask for.
  const carriesArrangement = next.some(
    (node) => node.target === null || node.children.length > 0
  );
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
    layoutRev = saved.revision;
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
// Every one is a pure function over the tree, addressed by PATH (the chain of
// child indices). The sidebar draws one flat run of visible rows, so a drag
// speaks in visible-row indices and a depth; `resolveDrop` in favoritesLayout
// turns that pair into a (parent, index) before anything is mutated.

/** Move the row at `from` under `parent` at `index`. */
export function moveFavoriteRow(
  next: FavLayout,
  from: number[],
  parent: number[],
  index: number,
): FavLayout {
  return moveNode(next, from, parent, index);
}

/** Add a label at the top level. */
export function addGroup(next: FavLayout, desired = "New group"): FavLayout {
  return [...next, labelNode(uniqueGroupName(next, desired))];
}

export function renameGroup(next: FavLayout, path: number[], name: string): FavLayout {
  const trimmed = name.trim();
  const node = nodeAt(next, path);
  if (!trimmed || !node || node.target !== null) return next;
  // Uniqueness is checked against every OTHER label, so re-committing an
  // unchanged name does not append " 2" to it.
  const without = promoteChildrenAt(next, path);
  return updateAt(next, path, (target) => [
    { ...target, raw: uniqueGroupName(without, trimmed) },
  ]);
}

/** Delete a label WITHOUT unfavoriting anything: its children take its place.
 *  Capacities states this contract explicitly and it is the one users rely on;
 *  Obsidian's bookmark folders lose the reference instead. */
export function deleteGroup(next: FavLayout, path: number[]): FavLayout {
  const node = nodeAt(next, path);
  if (!node || node.target !== null) return next;
  return promoteChildrenAt(next, path);
}

export function setGroupCollapsed(
  next: FavLayout,
  path: number[],
  collapsed: boolean,
): FavLayout {
  return updateAt(next, path, (target) => [
    { ...target, collapsed: collapsed || undefined },
  ]);
}
