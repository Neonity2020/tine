// The Favorites arrangement — nesting and order — lives in an ordinary graph
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
// sees the favorites flat, in the arrangement's pre-order.
//
// The page shape is the plainest thing that round-trips — ordinary bullets,
// nested to any depth:
//
//     tine/favorites:: true
//
//     - [[Alpha]]            <- a favorite
//     - Work                 <- a label (plain text, NOT a page)
//     	- [[Beta]]
//     	- Active             <- labels nest
//     		- [[Gamma]]
//     - [[Delta]]            <- favorites nest under favorites too
//     	- [[Epsilon]]
//
// ONE node type, not "groups containing items". A bullet is a favorite when it
// is exactly one `[[link]]` and a label otherwise; both nest, both carry their
// children. That is what the page already is, so nothing has to be flattened on
// read or invented on write — which is the whole reason arbitrary depth costs
// nothing here. A label is deliberately not a page: making it one would couple
// the sidebar's structure to page lifecycle (rename it? delete it? does
// clicking it open the page or collapse the subtree?) for no benefit.
import type { BlockDto, PageKind } from "./types";
import { isJournalTitle } from "./journal";
import { pageIdentityKey } from "./pageIdentity";

export const FAVORITES_PAGE_PROPERTY = "tine/favorites";
export const DEFAULT_FAVORITES_PAGE = "Favorites";

/** One bullet of the arrangement page. */
export interface FavNode {
  /** The page this row points at, when the bullet is exactly one `[[link]]`;
   *  `null` for a label. This is the ONLY distinction between the two. */
  target: string | null;
  /** The bullet's text, verbatim. Round-tripped for every node, so a bullet the
   *  user wrote by hand survives whatever Tine writes next — at any depth. */
  raw: string;
  collapsed?: boolean;
  children: FavNode[];
}

export type FavLayout = FavNode[];

export interface FavLayoutItem {
  name: string;
  kind: PageKind;
}

/** A visible row: a node, where it lives, and how deep it is drawn. */
export interface FavRow {
  path: number[];
  node: FavNode;
  depth: number;
}

const LINK_ONLY = /^\s*\[\[([^\]]+)\]\]\s*$/;

/** The page name a bullet points at, when the bullet is nothing but one link. */
export function linkOnlyTarget(raw: string): string | null {
  const match = LINK_ONLY.exec(raw);
  const name = match?.[1]?.trim();
  return name ? name : null;
}

export const itemKind = (name: string): PageKind =>
  isJournalTitle(name) ? "journal" : "page";

export function favoriteNode(name: string): FavNode {
  return { target: name, raw: `[[${name}]]`, children: [] };
}

export function labelNode(name: string): FavNode {
  return { target: null, raw: name, children: [] };
}

export function emptyLayout(): FavLayout {
  return [];
}

/** Read the arrangement out of the page's block tree. */
export function layoutFromBlocks(roots: BlockDto[]): FavLayout {
  const read = (blocks: BlockDto[]): FavNode[] =>
    blocks.flatMap((block) => {
      const raw = block.raw.trim();
      const children = read(block.children);
      if (!raw) {
        // A blank bullet names nothing and cannot be rendered or edited as a
        // label. Drop it, but never its children — they are real rows.
        return children;
      }
      return [
        {
          target: linkOnlyTarget(block.raw),
          raw,
          collapsed: block.collapsed || undefined,
          children,
        },
      ];
    });
  return read(roots);
}

/** Serialize back to Markdown bullets (tab-indented children, matching how Tine
 *  writes every other page). */
export function layoutToMarkdown(layout: FavLayout): string {
  const lines: string[] = [];
  const emit = (nodes: FavNode[], depth: number) => {
    for (const node of nodes) {
      lines.push(`${"\t".repeat(depth)}- ${node.raw}`);
      emit(node.children, depth + 1);
    }
  };
  emit(layout, 0);
  return lines.length ? `${lines.join("\n")}\n` : "";
}

/** Flat membership in display (pre-order) order — exactly what is projected
 *  one-way into `config.edn :favorites`. */
export function layoutMembers(layout: FavLayout): FavLayoutItem[] {
  const out: FavLayoutItem[] = [];
  const walk = (nodes: FavNode[]) => {
    for (const node of nodes) {
      if (node.target) out.push({ name: node.target, kind: itemKind(node.target) });
      walk(node.children);
    }
  };
  walk(layout);
  return out;
}

// Same identity fold as membership (`favoriteKey` in ui.ts) — arrangement and
// membership must never disagree on whether two spellings are one favorite
// (DUP-2: this was a weaker trim+toLowerCase while membership was exact-match).
const keyOf = (name: string) => pageIdentityKey(name);

/** Fold an externally-observed membership list (config.edn, i.e. what Logseq
 *  may have changed) back into the arrangement:
 *
 *  - a name that vanished from membership is removed wherever it sat, and its
 *    children take its place rather than disappearing with it. Tine's
 *    arrangement must never resurrect an unfavorited page, and must never lose
 *    a favorite for the unrelated reason that its parent was unstarred;
 *  - a name membership has that the arrangement does not is appended at the
 *    TOP level, preserving the order it arrived in;
 *  - labels are always kept: they are the user's own text, not membership;
 *  - everything already arranged keeps its place.
 *
 *  Re-favoriting a removed page therefore lands it at the end rather than
 *  silently restoring an old slot, which is what a user who just removed it
 *  expects. */
export function reconcileLayout(layout: FavLayout, membership: string[]): FavLayout {
  const wanted = new Map(membership.map((name) => [keyOf(name), name] as const));
  const seen = new Set<string>();
  const keep = (nodes: FavNode[]): FavNode[] =>
    nodes.flatMap((node) => {
      const children = keep(node.children);
      if (node.target === null) return [{ ...node, children }];
      const key = keyOf(node.target);
      if (!wanted.has(key) || seen.has(key)) return children;
      seen.add(key);
      return [{ ...node, children }];
    });
  const next = keep(layout);
  for (const [key, name] of wanted) {
    if (!seen.has(key)) next.push(favoriteNode(name));
  }
  return next;
}

/** A label the user can add without colliding with an existing one. */
export function uniqueGroupName(layout: FavLayout, desired: string): string {
  const taken = new Set<string>();
  const walk = (nodes: FavNode[]) => {
    for (const node of nodes) {
      if (node.target === null) taken.add(keyOf(node.raw));
      walk(node.children);
    }
  };
  walk(layout);
  if (!taken.has(keyOf(desired))) return desired;
  for (let n = 2; ; n += 1) {
    const candidate = `${desired} ${n}`;
    if (!taken.has(keyOf(candidate))) return candidate;
  }
}

// ---------------------------------------------------------------------------
// Paths and rows.
//
// A path is the chain of child indices from the root, so it addresses a node
// without holding a reference to it — which matters because every mutation
// rebuilds the nodes it touches.

export function nodeAt(layout: FavLayout, path: number[]): FavNode | null {
  let nodes = layout;
  let found: FavNode | null = null;
  for (const index of path) {
    found = nodes[index] ?? null;
    if (!found) return null;
    nodes = found.children;
  }
  return found;
}

/** Rebuild one node in place — replacing it with zero, one, or several nodes —
 *  touching only its ancestors. Every structural mutation goes through this, so
 *  removal, promotion and edit-in-place cannot drift apart. */
export function updateAt(
  layout: FavLayout,
  path: number[],
  update: (node: FavNode) => FavNode[],
): FavLayout {
  if (!path.length) return layout;
  const [head, ...rest] = path;
  return layout.flatMap((node, i) => {
    if (i !== head) return [node];
    if (!rest.length) return update(node);
    return [{ ...node, children: updateAt(node.children, rest, update) }];
  });
}

export function removeAt(layout: FavLayout, path: number[]): FavLayout {
  return updateAt(layout, path, () => []);
}

/** Delete a row WITHOUT losing what it held: its children take its place. */
export function promoteChildrenAt(layout: FavLayout, path: number[]): FavLayout {
  return updateAt(layout, path, (node) => node.children);
}

/** Insert `node` as the `index`th child of `parent` (`[]` for the top level). */
export function insertAt(
  layout: FavLayout,
  parent: number[],
  index: number,
  node: FavNode,
): FavLayout {
  if (!parent.length) {
    const next = [...layout];
    next.splice(Math.max(0, Math.min(index, next.length)), 0, node);
    return next;
  }
  return updateAt(layout, parent, (target) => [
    {
      ...target,
      // Dropping into a collapsed row would hide the thing the user just moved.
      collapsed: undefined,
      children: insertAt(target.children, [], index, node),
    },
  ]);
}

/** The rows the sidebar actually draws: pre-order, skipping what a collapsed
 *  row hides. The visible index IS `data-row-index`, so a drop target found by
 *  `elementFromPoint` needs no translation. */
export function visibleRows(layout: FavLayout): FavRow[] {
  const rows: FavRow[] = [];
  const walk = (nodes: FavNode[], prefix: number[], depth: number) => {
    nodes.forEach((node, i) => {
      const path = [...prefix, i];
      rows.push({ path, node, depth });
      if (!node.collapsed) walk(node.children, path, depth + 1);
    });
  };
  walk(layout, [], 0);
  return rows;
}

const startsWith = (path: number[], prefix: number[]) =>
  prefix.every((value, i) => path[i] === value);

/** Is `path` the dragged node or inside it? Such a row is not a legal drop
 *  target: a node cannot become its own descendant. */
export const isWithin = (path: number[], root: number[]) =>
  path.length >= root.length && startsWith(path, root);

/** Where a path moves to once `removed` is gone. `null` when it was inside the
 *  removed subtree and no longer exists. */
export function adjustPathAfterRemoval(path: number[], removed: number[]): number[] | null {
  if (isWithin(path, removed)) return null;
  const at = removed.length - 1;
  if (path.length >= removed.length && startsWith(path, removed.slice(0, at)) && path[at] > removed[at]) {
    const next = [...path];
    next[at] -= 1;
    return next;
  }
  return [...path];
}

/** The depths a drop into slot `slot` may legally use.
 *
 *  The outliner rule: no deeper than one level under the row above, and no
 *  shallower than the row below — otherwise the row below would silently change
 *  parent. With nothing above, the only legal depth is the top level. */
export function depthRange(rows: FavRow[], slot: number): { min: number; max: number } {
  const above = rows[slot - 1];
  const below = rows[slot];
  const max = above ? above.depth + 1 : 0;
  const min = below ? Math.min(below.depth, max) : 0;
  return { min, max };
}

/** Resolve a drop — slot between visible rows, plus the depth the pointer's
 *  horizontal position asked for — into (parent, index). */
export function resolveDrop(
  rows: FavRow[],
  slot: number,
  desiredDepth: number,
): { parent: number[]; index: number } {
  const { min, max } = depthRange(rows, slot);
  const depth = Math.max(min, Math.min(desiredDepth, max));
  if (depth === 0) {
    // Count the top-level rows that end up before the slot.
    const index = rows.slice(0, slot).filter((row) => row.depth === 0).length;
    return { parent: [], index };
  }
  // The nearest row above at the parent's depth owns this drop.
  for (let i = slot - 1; i >= 0; i -= 1) {
    if (rows[i].depth !== depth - 1) continue;
    const parent = rows[i].path;
    const index = rows
      .slice(i + 1, slot)
      .filter((row) => row.path.length === parent.length + 1 && startsWith(row.path, parent))
      .length;
    return { parent, index };
  }
  return { parent: [], index: rows.slice(0, slot).filter((row) => row.depth === 0).length };
}

/** Move the node at `from` under `parent` at `index`.
 *
 *  `from` and `parent` are paths in the tree BEFORE the move; `index` is the
 *  position among `parent`'s children AFTER the node has been lifted out. That
 *  split is deliberate: `resolveDrop` counts the rows still standing once the
 *  dragged subtree is excluded, so it already speaks post-removal, and
 *  re-adjusting here would move a same-parent drop one slot too far. */
export function moveNode(
  layout: FavLayout,
  from: number[],
  parent: number[],
  index: number,
): FavLayout {
  const node = nodeAt(layout, from);
  if (!node || isWithin(parent, from)) return layout;
  const adjustedParent = adjustPathAfterRemoval(parent, from);
  if (!adjustedParent) return layout;
  return insertAt(removeAt(layout, from), adjustedParent, index, node);
}
