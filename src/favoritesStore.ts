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
