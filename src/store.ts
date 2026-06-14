// The live editing tree. The frontend owns this during a session; all
// keystrokes and structural ops mutate it synchronously (zero IPC). Persistence
// is a debounced whole-page save to Rust. See plan §"block editor model".
//
// Blocks are stored normalized (byId + parent/children ids) so reparenting on
// indent/outdent is a cheap array move. Caret is tracked as (blockId, offset)
// in raw-markdown coordinates and restored after structural ops.

import { createStore, produce } from "solid-js/store";
import { createSignal } from "solid-js";
import type { BlockDto, PageDto, PageKind } from "./types";
import { backend } from "./backend";

export interface Node {
  id: string;
  raw: string;
  collapsed: boolean;
  parent: string | null; // null = root
  children: string[];
}

interface PageState {
  name: string;
  kind: PageKind;
  title: string;
  preBlock: string | null;
  byId: Record<string, Node>;
  roots: string[];
  loaded: boolean;
}

const empty: PageState = {
  name: "",
  kind: "page",
  title: "",
  preBlock: null,
  byId: {},
  roots: [],
  loaded: false,
};

export const [page, setPage] = createStore<PageState>({ ...empty });

// Which block is currently being edited (textarea shown), and where to put the
// caret once that editor mounts.
export const [editingId, setEditingId] = createSignal<string | null>(null);
const [caretTarget, setCaretTarget] = createSignal<{ id: string; offset: number } | null>(null);

export function takeCaretFor(id: string): number | null {
  const t = caretTarget();
  if (t && t.id === id) {
    setCaretTarget(null);
    return t.offset;
  }
  return null;
}

let idCounter = 0;
function freshId(): string {
  return `b${Date.now().toString(36)}-${idCounter++}`;
}

// ---------------------------------------------------------------------------
// Loading / serializing
// ---------------------------------------------------------------------------

function flatten(dtos: BlockDto[], parent: string | null, byId: Record<string, Node>): string[] {
  return dtos.map((d) => {
    const childIds = flatten(d.children, d.id, byId);
    byId[d.id] = { id: d.id, raw: d.raw, collapsed: d.collapsed, parent, children: childIds };
    return d.id;
  });
}

export function loadPageDto(dto: PageDto) {
  const byId: Record<string, Node> = {};
  const roots = flatten(dto.blocks, null, byId);
  setPage({
    name: dto.name,
    kind: dto.kind,
    title: dto.title,
    preBlock: dto.pre_block,
    byId,
    roots,
    loaded: true,
  });
  setEditingId(null);
}

function toDto(id: string): BlockDto {
  const n = page.byId[id];
  return { id: n.id, raw: n.raw, collapsed: n.collapsed, children: n.children.map(toDto) };
}

export function toPageDto(): PageDto {
  return {
    name: page.name,
    kind: page.kind,
    title: page.title,
    pre_block: page.preBlock,
    blocks: page.roots.map(toDto),
  };
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

function siblingsOf(id: string): string[] {
  const p = page.byId[id].parent;
  return p === null ? page.roots : page.byId[p].children;
}

function indexInSiblings(id: string): number {
  return siblingsOf(id).indexOf(id);
}

/** Visible blocks in display order (skips children of collapsed blocks). */
export function visibleOrder(): string[] {
  const out: string[] = [];
  const walk = (ids: string[]) => {
    for (const id of ids) {
      out.push(id);
      const n = page.byId[id];
      if (!n.collapsed && n.children.length) walk(n.children);
    }
  };
  walk(page.roots);
  return out;
}

export function prevVisible(id: string): string | null {
  const order = visibleOrder();
  const i = order.indexOf(id);
  return i > 0 ? order[i - 1] : null;
}

export function nextVisible(id: string): string | null {
  const order = visibleOrder();
  const i = order.indexOf(id);
  return i >= 0 && i < order.length - 1 ? order[i + 1] : null;
}

export function depthOf(id: string): number {
  let d = 0;
  let p = page.byId[id]?.parent ?? null;
  while (p !== null) {
    d++;
    p = page.byId[p].parent;
  }
  return d;
}

// ---------------------------------------------------------------------------
// Mutations (each schedules a debounced save)
// ---------------------------------------------------------------------------

export function setRaw(id: string, raw: string) {
  setPage("byId", id, "raw", raw);
  scheduleSave();
}

export function startEditing(id: string, offset: number) {
  setCaretTarget({ id, offset });
  setEditingId(id);
}

/** Enter: split the block at `offset`. */
export function splitBlock(id: string, offset: number) {
  const node = page.byId[id];
  const before = node.raw.slice(0, offset);
  const after = node.raw.slice(offset);
  const newId = freshId();

  setPage(
    produce((s) => {
      s.byId[id].raw = before;
      const hasVisibleChildren = node.children.length > 0 && !node.collapsed;
      if (hasVisibleChildren) {
        // New block becomes the first child of the (expanded) parent.
        s.byId[newId] = { id: newId, raw: after, collapsed: false, parent: id, children: [] };
        s.byId[id].children.unshift(newId);
      } else {
        // New block becomes the next sibling.
        s.byId[newId] = {
          id: newId,
          raw: after,
          collapsed: false,
          parent: node.parent,
          children: [],
        };
        const sibs = node.parent === null ? s.roots : s.byId[node.parent].children;
        sibs.splice(sibs.indexOf(id) + 1, 0, newId);
      }
    })
  );
  startEditing(newId, 0);
  scheduleSave();
}

/** Tab: make the block the last child of its previous sibling. */
export function indentBlock(id: string, caretOffset: number) {
  const i = indexInSiblings(id);
  if (i <= 0) return; // no previous sibling
  const sibs = siblingsOf(id);
  const newParent = sibs[i - 1];
  setPage(
    produce((s) => {
      const arr = s.byId[id].parent === null ? s.roots : s.byId[s.byId[id].parent!].children;
      arr.splice(arr.indexOf(id), 1);
      s.byId[id].parent = newParent;
      s.byId[newParent].children.push(id);
      s.byId[newParent].collapsed = false;
    })
  );
  startEditing(id, caretOffset);
  scheduleSave();
}

/** Shift+Tab: move the block out to be the next sibling of its parent.
 *  Following siblings become children of the moved block (Logseq behavior). */
export function outdentBlock(id: string, caretOffset: number) {
  const node = page.byId[id];
  if (node.parent === null) return; // already a root
  const parentId = node.parent;
  const parentNode = page.byId[parentId];
  const grandParent = parentNode.parent;

  setPage(
    produce((s) => {
      const parent = s.byId[parentId];
      const idx = parent.children.indexOf(id);
      // Detach id and its following siblings.
      const following = parent.children.splice(idx); // [id, ...rest]
      following.shift(); // drop id itself
      parent.children = parent.children; // (already mutated)
      // Append following siblings as children of id.
      for (const f of following) {
        s.byId[f].parent = id;
      }
      s.byId[id].children.push(...following);
      // Insert id into grandparent right after parent.
      s.byId[id].parent = grandParent;
      const gArr = grandParent === null ? s.roots : s.byId[grandParent].children;
      gArr.splice(gArr.indexOf(parentId) + 1, 0, id);
    })
  );
  startEditing(id, caretOffset);
  scheduleSave();
}

/** Backspace at offset 0: merge into the previous visible block. */
export function mergeWithPrev(id: string): boolean {
  const prev = prevVisible(id);
  if (prev === null) return false;
  const node = page.byId[id];
  // Don't merge into an ancestor that would swallow this block's own subtree
  // ambiguously; Logseq still merges text into the previous visible block.
  const prevRaw = page.byId[prev].raw;
  const joinOffset = prevRaw.length;

  setPage(
    produce((s) => {
      s.byId[prev].raw = prevRaw + node.raw;
      // Reparent this block's children to the end of prev's children.
      for (const c of node.children) s.byId[c].parent = prev;
      s.byId[prev].children.push(...node.children);
      // Remove id from its siblings.
      const arr = node.parent === null ? s.roots : s.byId[node.parent].children;
      arr.splice(arr.indexOf(id), 1);
      delete s.byId[id];
    })
  );
  startEditing(prev, joinOffset);
  scheduleSave();
  return true;
}

export function toggleCollapse(id: string) {
  const n = page.byId[id];
  if (n.children.length === 0) return;
  setPage("byId", id, "collapsed", !n.collapsed);
  scheduleSave();
}

// ---------------------------------------------------------------------------
// Debounced persistence
// ---------------------------------------------------------------------------

let saveTimer: ReturnType<typeof setTimeout> | null = null;
export function scheduleSave() {
  if (!page.loaded) return;
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    void backend().savePage(toPageDto());
  }, 400);
}
