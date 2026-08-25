// The Favorites arrangement — groups and order — lives in an ordinary graph
// page, named by `:tine/favorites-page` in config.edn.
//
// Why a page rather than a JSON/EDN blob: a blob has no merge semantics, so two
// devices editing favorites is last-writer-wins, which is the documented cause
// of Obsidian's total bookmark loss across Sync. A page is a block tree, so it
// merges deterministically through the oplog on Managed Storage and reaches the
// Concord conflict UI on Direct Files, for free. It also costs no new write
// path: it saves through the audited `save_page` like any other page.
//
// Why `[[links]]` rather than plain text: renames follow them for free. GH #79
// had to fix rename-tracking for the flat `:favorites` list by hand; links make
// that class of staleness structural rather than something to remember.
//
// Membership stays in `config.edn :favorites` as a flat, Logseq-readable list,
// written as a ONE-WAY projection of this page. Logseq keeps working; it just
// sees the favorites flat and in order.
//
// The page shape is deliberately the plainest thing that round-trips:
//
//     tine/favorites:: true
//
//     - [[Alpha]]            <- ungrouped favorite
//     - Work                 <- group header (plain text, NOT a page)
//       - [[Beta]]
//       - [[Gamma]]
//
// A group header is plain text on purpose. Making it a page would couple the
// sidebar's structure to page lifecycle — rename it? delete it? does clicking
// it open the page or collapse the group? — for no benefit.
import type { BlockDto, PageKind } from "./types";
import { isJournalTitle } from "./journal";

export const FAVORITES_PAGE_PROPERTY = "tine/favorites";
export const DEFAULT_FAVORITES_PAGE = "Favorites";

export interface FavLayoutItem {
  name: string;
  kind: PageKind;
}

export interface FavLayoutGroup {
  /** `null` is the ungrouped section, which is always first and may be empty. */
  name: string | null;
  items: FavLayoutItem[];
  /** Verbatim raws of child blocks we did not recognize, re-emitted after the
   *  items so a user editing this page by hand never loses what they wrote. */
  passthrough: string[];
  collapsed?: boolean;
}

export type FavLayout = FavLayoutGroup[];

const LINK_ONLY = /^\s*\[\[([^\]]+)\]\]\s*$/;

/** The page name a bullet points at, when the bullet is nothing but one link. */
export function linkOnlyTarget(raw: string): string | null {
  const match = LINK_ONLY.exec(raw);
  const name = match?.[1]?.trim();
  return name ? name : null;
}

const itemFor = (name: string): FavLayoutItem => ({
  name,
  kind: isJournalTitle(name) ? "journal" : "page",
});

export function emptyLayout(): FavLayout {
  return [{ name: null, items: [], passthrough: [] }];
}

/** Read the arrangement out of the page's block tree. Anything that is not a
 *  link-only bullet or a group header is preserved verbatim rather than
 *  discarded — this is a real page and the user may edit it. */
export function layoutFromBlocks(roots: BlockDto[]): FavLayout {
  const ungrouped: FavLayoutGroup = { name: null, items: [], passthrough: [] };
  const layout: FavLayout = [ungrouped];
  for (const block of roots) {
    const target = linkOnlyTarget(block.raw);
    if (target) {
      ungrouped.items.push(itemFor(target));
      // A link-only bullet with children is still just a favorite; its children
      // are not members of a group, so keep them beside the ungrouped section.
      for (const child of block.children) ungrouped.passthrough.push(child.raw);
      continue;
    }
    const heading = block.raw.trim();
    if (!heading) {
      // A blank bullet is not a group. Dropping it is the one deletion we make,
      // and only because an empty group header would be unnameable in the UI.
      continue;
    }
    const group: FavLayoutGroup = {
      name: heading,
      items: [],
      passthrough: [],
      collapsed: block.collapsed || undefined,
    };
    for (const child of block.children) {
      const childTarget = linkOnlyTarget(child.raw);
      if (childTarget) group.items.push(itemFor(childTarget));
      else group.passthrough.push(child.raw);
    }
    layout.push(group);
  }
  return layout;
}

/** Serialize the arrangement back to Markdown bullets (tab-indented children,
 *  matching how Tine writes every other page). */
export function layoutToMarkdown(layout: FavLayout): string {
  const lines: string[] = [];
  for (const group of layout) {
    if (group.name === null) {
      for (const item of group.items) lines.push(`- [[${item.name}]]`);
      for (const raw of group.passthrough) lines.push(`- ${raw}`);
      continue;
    }
    lines.push(`- ${group.name}`);
    for (const item of group.items) lines.push(`\t- [[${item.name}]]`);
    for (const raw of group.passthrough) lines.push(`\t- ${raw}`);
  }
  return lines.length ? `${lines.join("\n")}\n` : "";
}

/** Flat membership in display order — exactly what is projected one-way into
 *  `config.edn :favorites`. */
export function layoutMembers(layout: FavLayout): FavLayoutItem[] {
  return layout.flatMap((group) => group.items);
}

const keyOf = (name: string) => name.trim().toLowerCase();

/** Fold an externally-observed membership list (config.edn, i.e. what Logseq
 *  may have changed) back into the arrangement:
 *
 *  - a name that vanished from membership is removed from its group, wherever
 *    it sat. Tine's arrangement must never resurrect an unfavorited page;
 *  - a name membership has that the arrangement does not is appended to the
 *    UNGROUPED section, preserving the relative order it arrived in;
 *  - everything the user already arranged keeps its group and position.
 *
 *  Re-favoriting a removed page therefore lands it at the end of ungrouped
 *  rather than silently restoring an old slot, which is what a user who just
 *  removed it expects. */
export function reconcileLayout(layout: FavLayout, membership: string[]): FavLayout {
  const wanted = new Map(membership.map((name) => [keyOf(name), name] as const));
  const seen = new Set<string>();
  const next: FavLayout = layout.map((group) => ({
    ...group,
    items: group.items.filter((item) => {
      const key = keyOf(item.name);
      if (!wanted.has(key) || seen.has(key)) return false;
      seen.add(key);
      return true;
    }),
  }));
  if (!next.length || next[0].name !== null) {
    next.unshift({ name: null, items: [], passthrough: [] });
  }
  for (const [key, name] of wanted) {
    if (!seen.has(key)) next[0].items.push(itemFor(name));
  }
  return next;
}

/** A group name the user can add without colliding with an existing one. */
export function uniqueGroupName(layout: FavLayout, desired: string): string {
  const taken = new Set(
    layout.filter((g) => g.name !== null).map((g) => keyOf(g.name!))
  );
  if (!taken.has(keyOf(desired))) return desired;
  for (let n = 2; ; n += 1) {
    const candidate = `${desired} ${n}`;
    if (!taken.has(keyOf(candidate))) return candidate;
  }
}
