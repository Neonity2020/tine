import type { HistoryEditorContext } from "../editorController";
import type { HistorySidebarContext } from "../ui";
import type { Route } from "../router";
import { FeedPage, Node, doc, hasLoadedIdentityCollision, pageByName, pageInstanceGeneration, pageWritable, peekPageInstanceGeneration, purgePageNodes, setDoc, storeMutationObserverForTest } from "./doc";
import { addDirty, scheduleSave } from "../persistence";
import { persistCrossPage } from "../crossPageMove";
import { captureHistoryEditorContext, captureRawHistoryViewport, editingId, endEdit, restoreHistoryEditorContext } from "../editorController";
import { captureHistorySidebarContext, pushToast, restoreHistorySidebarContext } from "../ui";
import { createSignal } from "solid-js";
import { invalidateAllMatrixDimensions } from "../sheet/matrix";
import { produce, unwrap } from "solid-js/store";
import { recordClipboardUndoSnapshotForTest } from "../clipboardWorkProbe";


// ---------------------------------------------------------------------------
// Undo / redo (snapshot-based; typing in one block coalesces to one step)
// ---------------------------------------------------------------------------

// A page-scoped structural snapshot, or a single-block raw patch (typing).
// Typing is by far the most frequent op, so it records an O(1) inverse instead
// of cloning anything. A structural op snapshots ONLY the pages it touches (its
// nodes + page objects), so the cost is O(edited page), not O(whole working set)
// — a structural edit no longer slows down as more journal days / sidebar / query
// pages get loaded. `pages: null` means "all loaded pages" (the safe fallback for
// an op that can't declare its scope).
interface SnapEntry {
  kind: "snap";
  /** {@link UndoEntry}'s optional tag: the name `pushUndo` was called with. */
  tag?: string;
  pages: string[] | null; // affected page names (null = whole working set)
  pageObjs: FeedPage[]; // snapshot of those pages' FeedPage objects
  nodes: Record<string, Node>; // snapshot of nodes living on those pages
  dirty: string[]; // pages to re-save on undo/redo
  context: HistoryContext;
  /** Page-instance generations this entry was recorded against (GH #305). */
  instances: Record<string, number>;
  /** Identity-bearing clipboard paste whose redo must fail on a live conflict. */
  preservedIds?: string[];
}
interface RawEntry {
  kind: "raw";
  tag?: string;
  id: string;
  raw: string; // the block's text to restore
  page: string;
  /** A transient page-header node can legitimately disappear when its text is
   * deleted. Carry the structural shell on its normal O(1) typing undo entry so
   * Undo can restore it and Redo can remove it again without an extra step. */
  headerRoot?: { node: Node; rootIndex: number };
  removeHeaderOnApply?: boolean;
  context: HistoryContext;
  /** Page-instance generations this entry was recorded against (GH #305). */
  instances: Record<string, number>;
  preservedIds?: string[];
}
/** **The tag is on the ENTRY, not in a module global.** `lastUndoTag` is a
 *  typing-coalesce marker that any unrelated event resets, so it cannot answer
 *  "is the change Undo would take back the one I am offering to take back?" —
 *  the question the §7.5 crossing notice has to ask before it enables its
 *  button. {@link undoTopTag} answers it off the entry itself. */
type UndoEntry = SnapEntry | RawEntry;
const undoStack: UndoEntry[] = [];
let redoStack: UndoEntry[] = [];
let lastUndoTag: string | null = null;
let undoSuppressionDepth = 0;

/**
 * Repeated selection moves have a deliberately narrower coalescing rule than
 * ordinary structural commands. Every key repeat still changes the live Solid
 * document immediately; this ledger only lets the repeats share their first
 * page-scoped undo snapshot.
 */
interface MoveSelectionBurst {
  /** Ordered top-level selection roots. A set is insufficient: order is state. */
  roots: string[];
  /** Sorted exact page-instance scope captured by the first nudge. */
  pages: Array<{ name: string; instance: number }>;
  /** Prevents reuse across Undo/Redo/history replacement. */
  historyEpoch: number;
  startedAt: number;
  lastCommandAt: number;
  idleTimer: ReturnType<typeof setTimeout> | null;
}

let moveSelectionBurst: MoveSelectionBurst | null = null;
let historyEpoch = 0;
const MOVE_SELECTION_BURST_IDLE_MS = 400;
const MOVE_SELECTION_BURST_MAX_MS = 3_000;

function endMoveSelectionBurst(): void {
  const idleTimer = moveSelectionBurst?.idleTimer;
  if (idleTimer !== null && idleTimer !== undefined) clearTimeout(idleTimer);
  moveSelectionBurst = null;
}

/** Bumped on every mutation of the undo/redo stacks (and on the mode toggle,
 *  which changes WHICH entry a pop would select). {@link undoTopTag} reads it so
 *  an affordance that offers to undo one specific change re-renders the moment
 *  that stops being true. The stacks themselves are plain arrays on purpose —
 *  making them reactive would put a proxy on the editor's hottest path. */
const [historyRev, setHistoryRev] = createSignal(0);
let historyRevScheduled = false;
/** **Deferred by one microtask, deliberately.** `advanceHistoryEpoch` runs
 *  BEFORE its caller mutates a stack — `pushUndo` pushes after it,
 *  `performUndo` pops after it, `invalidateUndoForPage` splices after it — so a
 *  synchronous notification would hand every reader the stack as it was, which
 *  is exactly the wrong answer. One microtask later every one of those
 *  mutations has landed, and a burst collapses into a single notification. */
function bumpHistoryRev(): void {
  if (historyRevScheduled) return;
  historyRevScheduled = true;
  queueMicrotask(() => {
    historyRevScheduled = false;
    setHistoryRev((n) => n + 1);
  });
}

function advanceHistoryEpoch(): void {
  historyEpoch++;
  bumpHistoryRev();
}

function armMoveSelectionBurstIdleTimer(burst: MoveSelectionBurst): void {
  if (burst.idleTimer !== null) clearTimeout(burst.idleTimer);
  burst.idleTimer = setTimeout(() => {
    // A later repeat arms a different timer; an obsolete timer must not close it.
    if (moveSelectionBurst !== burst) return;
    if (Date.now() - burst.lastCommandAt >= MOVE_SELECTION_BURST_IDLE_MS) endMoveSelectionBurst();
  }, MOVE_SELECTION_BURST_IDLE_MS);
}

function currentMoveSelectionScope(ids: readonly string[]): Array<{ name: string; instance: number }> | null {
  const names = [...new Set(ids.map((id) => doc.byId[id]?.page).filter(Boolean) as string[])].sort();
  const pages: Array<{ name: string; instance: number }> = [];
  for (const name of names) {
    const instance = pageInstanceGeneration(name);
    if (instance === null) return null;
    pages.push({ name, instance });
  }
  return pages;
}

function sameMoveSelectionScope(
  left: readonly { name: string; instance: number }[],
  right: readonly { name: string; instance: number }[],
): boolean {
  return left.length === right.length
    && left.every((page, index) => page.name === right[index]?.name && page.instance === right[index]?.instance);
}

/**
 * Start a fresh selection-move undo gesture, or reuse its first snapshot for a
 * matching short repeat. Up and Down deliberately share this command family:
 * reversing direction is still one continuous selection-move gesture.
 */
function beginOrContinueMoveSelectionUndo(ids: readonly string[]): string[] | null {
  const pages = currentMoveSelectionScope(ids);
  if (!pages?.length) return null;
  const now = Date.now();
  const current = moveSelectionBurst;
  const matching = current
    && current.historyEpoch === historyEpoch
    && current.roots.length === ids.length
    && current.roots.every((id, index) => id === ids[index])
    && sameMoveSelectionScope(current.pages, pages)
    && now - current.lastCommandAt < MOVE_SELECTION_BURST_IDLE_MS
    && now - current.startedAt < MOVE_SELECTION_BURST_MAX_MS;
  if (matching) {
    current.lastCommandAt = now;
    armMoveSelectionBurstIdleTimer(current);
    return pages.map((page) => page.name);
  }

  endMoveSelectionBurst();
  // This one call opts out of the generic undo reset below. It is the only
  // structural command allowed to retain a previous snapshot.
  pushUndo("move-sel", pages.map((page) => page.name), undefined, { keepMoveSelectionBurst: true });
  const burst: MoveSelectionBurst = {
    roots: [...ids],
    pages,
    historyEpoch,
    startedAt: now,
    lastCommandAt: now,
    idleTimer: null,
  };
  moveSelectionBurst = burst;
  armMoveSelectionBurstIdleTimer(burst);
  return pages.map((page) => page.name);
}

// Session-scoped and global by default, matching OG's transient app-state flag
// at `src/main/frontend/state.cljs:304-306` (OG commit 6e7afa8eb).
let pageOnlyHistoryMode = false;

export function historyPageOnlyMode(): boolean {
  return pageOnlyHistoryMode;
}

export function toggleUndoRedoMode(): "Page only" | "Global" {
  pageOnlyHistoryMode = !pageOnlyHistoryMode;
  bumpHistoryRev();
  return pageOnlyHistoryMode ? "Page only" : "Global";
}

export interface HistoryRouteContext {
  paneId: string;
  route: Route;
}

let historyRouteContextAdapter: {
  capture: () => HistoryRouteContext | null;
  restore: (context: HistoryRouteContext) => boolean;
} = {
  capture: () => null,
  restore: () => false,
};

/** Router-owned adapter: keeps store.ts from adding a runtime import back to
 * router.ts (router already imports the store). */
export function installHistoryRouteContextAdapter(adapter: typeof historyRouteContextAdapter) {
  historyRouteContextAdapter = adapter;
}

interface HistoryContext {
  route: HistoryRouteContext | null;
  sidebar: HistorySidebarContext;
  editor: HistoryEditorContext | null;
}

/** Capture UI state at the same pre-mutation boundary as the data inverse. OG
 * stores app state on each history entity and cursor state by transaction at
 * `src/main/frontend/modules/editor/undo_redo.cljs:261-272` and
 * `src/main/frontend/modules/outliner/datascript.cljc:152-162`
 * (OG commit 6e7afa8eb). */
function captureHistoryContext(): HistoryContext {
  return {
    route: historyRouteContextAdapter.capture(),
    sidebar: captureHistorySidebarContext(),
    editor: captureHistoryEditorContext(),
  };
}

/** Discard all undo/redo history. Called on graph switch/reset so old-graph
 *  snapshots can't be replayed into a different graph. */
export function clearUndoHistory() {
  endMoveSelectionBurst();
  advanceHistoryEpoch();
  undoStack.length = 0;
  redoStack = [];
  lastUndoTag = null;
  undoSuppressionDepth = 0;
}

/** Does an undo entry reference page `name`? A raw entry by its `page`; a snap
 *  entry by its declared scope (a `null` scope = whole working set, so it touches
 *  every page including this one). */
function entryTouchesPage(e: UndoEntry, name: string): boolean {
  if (e.kind === "raw") return e.page === name;
  return e.pages === null || e.pages.includes(name);
}

/** The page-instance generations an entry is being recorded against.
 *
 *  An undo entry describes ONE loaded instance of each page it touches. Eviction
 *  deliberately keeps history, and re-opening the page installs a fresh instance
 *  carrying whatever the file says NOW — so replaying the old entry would restore
 *  pre-eviction content and mark the page dirty, and the save guard would accept
 *  it, because the baseline it submits under genuinely matches disk. Nothing in
 *  that path looks like a conflict. Stamping the generation makes the staleness
 *  visible at replay time, and covers every other way an instance is swapped
 *  (reload, rebind, forget) rather than only the reload-in-place path
 *  `invalidateUndoForPage` already handles. (GH #305) */
function captureInstances(names: readonly string[]): Record<string, number> {
  const instances: Record<string, number> = {};
  for (const name of names) {
    const generation = pageInstanceGeneration(name);
    if (generation !== null) instances[name] = generation;
  }
  return instances;
}

/** True when every page the entry describes is still the same loaded instance. */
function entryIsReplayable(e: UndoEntry): boolean {
  for (const name of Object.keys(e.instances)) {
    // peek, never the lazily-activating reader: minting a generation here would
    // make the check pass by inventing the identity it is supposed to compare.
    if (peekPageInstanceGeneration(name) !== e.instances[name]) return false;
  }
  return true;
}

/** Drop the popped stale entry's whole page history and say so once. Silence
 *  would read as "undo did nothing", which is how this class of bug hides. */
function discardStaleHistory(e: UndoEntry): void {
  for (const name of Object.keys(e.instances)) {
    if (peekPageInstanceGeneration(name) !== e.instances[name]) invalidateUndoForPage(name);
  }
  lastUndoTag = null;
  pushToast("Undo history for this page was discarded: the page was reloaded since those edits", "info");
}

/** The page owning the active editor wins over the focused pane's route. This is
 * OG's current/editing-page precedence at
 * `src/main/frontend/util/page.cljs:14-29` (OG commit 6e7afa8eb). */
function activeHistoryPage(): string | null {
  const id = editingId();
  const edited = id ? doc.byId[id] : undefined;
  if (edited) return edited.page;
  const route = historyRouteContextAdapter.capture()?.route;
  return route?.kind === "page" ? route.name : null;
}

/** The index of the entry a pop would take: the stack top in global mode, and
 * in page-only mode the NEWEST entry touching the active page — which is
 * generally not the top. Transcribes OG's filtered stack removal at
 * `src/main/frontend/modules/editor/undo_redo.cljs:81-106,132-156`
 * (OG commit 6e7afa8eb). `-1` when nothing would be taken.
 *
 * Selection and removal are one function so a caller that only wants to LOOK at
 * the next entry ({@link undoTopTag}) cannot drift from the one that pops it. */
function newestHistoryIndex(stack: UndoEntry[]): number {
  if (!stack.length) return -1;
  if (!pageOnlyHistoryMode) return stack.length - 1;
  const page = activeHistoryPage();
  if (!page) return stack.length - 1;
  for (let i = stack.length - 1; i >= 0; i--) {
    if (entryTouchesPage(stack[i], page)) return i;
  }
  return -1;
}

function popHistoryEntry(stack: UndoEntry[]): UndoEntry | undefined {
  const index = newestHistoryIndex(stack);
  return index < 0 ? undefined : stack.splice(index, 1)[0];
}

/** The tag of the entry {@link undo} would take back next, or null.
 *
 *  Used by an affordance that offers to undo ONE specific change it just made
 *  (the §7.5 crossing notice): if the answer is not that change's tag, the user
 *  has done something since, and pressing the button would take back the wrong
 *  edit. It reads the same entry `popHistoryEntry` would select, in both history
 *  modes, without popping it. */
export function undoTopTag(): string | null {
  historyRev();
  const index = newestHistoryIndex(undoStack);
  return index < 0 ? null : (undoStack[index].tag ?? null);
}

/** Drop undo/redo entries that reference `name`. Called when a page's on-disk
 *  content is reloaded under us (external edit → new baseRev) or the page is
 *  forgotten/deleted: a snapshot taken before that reload is stale, and replaying
 *  it would mark the page dirty and let autosave overwrite the external version —
 *  or, for a forgotten/deleted page, resurrect the file. We drop the whole entry
 *  (not just the page's slice) because a snap can't be partially applied; this can
 *  cost an unrelated co-snapshotted page its undo step, which is the safe tradeoff
 *  (lose an undo vs. clobber a file). */
export function invalidateUndoForPage(name: string) {
  // A page-instance replacement is a history boundary for the whole working
  // set, not only for bursts whose snapshot named that page. An unrelated
  // sidebar/watcher reload still changes the history episode; allowing an
  // active move gesture to span it can reuse a pre-reload snapshot afterwards.
  endMoveSelectionBurst();
  advanceHistoryEpoch();
  for (let i = undoStack.length - 1; i >= 0; i--) {
    if (entryTouchesPage(undoStack[i], name)) undoStack.splice(i, 1);
  }
  redoStack = redoStack.filter((e) => !entryTouchesPage(e, name));
  lastUndoTag = null; // don't coalesce a later edit onto a now-dropped entry
}

// Hand-rolled clones — Node/FeedPage are flat (primitives + a string[]), so a
// tailored copy is far cheaper than structuredClone (which probes types and
// walks for cycles). This runs on EVERY structural op (split/merge/indent/move/
// delete) for undo, so its cost is felt as general editor latency.
// Spread-based so a newly-added FeedPage/Node field can't be silently dropped from
// an undo snapshot (the trap that lost `path` — added for the #21 duplicate-day
// stray and read by pageToDto to pin the save to the exact file — so an undo/redo
// of a path-pinned page misrouted its next save to the canonical file). The only
// per-field work is deep-copying the one array each carries.
function cloneNode(n: Node): Node {
  return { ...n, children: n.children.slice() };
}
function clonePages(src: FeedPage[]): FeedPage[] {
  return src.map((p) => ({ ...p, roots: p.roots.slice() }));
}
function snapEntry(affected?: string[] | null, preservedIds?: readonly string[]): SnapEntry {
  // Vite replaces MODE at build time, so this diagnostic is dead-code-eliminated
  // from production snapshots rather than adding an observer check to undo.
  if (import.meta.env.MODE === "test") {
    storeMutationObserverForTest?.({ kind: "undo-snapshot" });
  }
  const context = captureHistoryContext();
  // null/omitted → snapshot the whole working set (safe fallback). Otherwise just
  // the named pages: their FeedPage objects + every node living on them.
  const names = affected ?? doc.pages.map((p) => p.name);
  const nameSet = new Set(names);
  const byId = unwrap(doc.byId);
  const pages = unwrap(doc.pages);
  const nodes: Record<string, Node> = {};
  // Collect each affected page's nodes by walking its root subtrees — O(nodes on
  // those pages), NOT O(whole loaded working set). A consistent pre-op tree has
  // every node-with-page-P reachable from P's roots (same invariant
  // purgePageNodes relies on), so this captures exactly the by-page set without
  // sweeping byId as sidebars/old journal days/query results accumulate.
  const visit = (id: string) => {
    const n = byId[id];
    if (!n || nodes[id]) return;
    nodes[id] = cloneNode(n);
    for (const c of n.children) visit(c);
  };
  for (const p of pages) {
    if (nameSet.has(p.name)) for (const r of p.roots) visit(r);
  }
  const pageObjs = clonePages(pages.filter((p) => nameSet.has(p.name)));
  if (import.meta.env.MODE === "test") {
    recordClipboardUndoSnapshotForTest(
      Object.keys(nodes).length,
      Object.values(nodes).reduce((total, node) => total + new TextEncoder().encode(node.raw).byteLength, 0),
    );
  }
  return {
    kind: "snap",
    pages: affected ?? null,
    pageObjs,
    nodes,
    dirty: names,
    context,
    instances: captureInstances(names),
    ...(preservedIds?.length ? { preservedIds: [...preservedIds] } : {}),
  };
}

/** Snapshot before a STRUCTURAL op. Pass the affected page name(s) so both the
 *  snapshot AND the undo re-save are scoped to just those pages; omit only when
 *  the op's page set isn't known (falls back to the whole working set — correct
 *  but O(loaded pages)). The affected set MUST include every page whose nodes the
 *  op changes, including a cross-page move's source AND destination, or undo
 *  would miss a page. `tag` resets the typing-coalesce marker. */
function pushUndo(
  tag: string,
  affected?: string[],
  preservedIds?: readonly string[],
  opts: { keepMoveSelectionBurst?: boolean } = {},
) {
  if (undoSuppressionDepth > 0) return;
  if (!opts.keepMoveSelectionBurst) endMoveSelectionBurst();
  advanceHistoryEpoch();
  undoStack.push({ ...snapEntry(affected, preservedIds), tag });
  if (undoStack.length > 200) undoStack.shift();
  redoStack = [];
  lastUndoTag = tag;
}

/** Record an O(1) inverse patch for a single-block text edit (typing). A typing
 *  burst in one block coalesces to a single entry holding the pre-burst text. */
function pushRawUndo(id: string, prevRaw: string) {
  if (undoSuppressionDepth > 0) return;
  endMoveSelectionBurst();
  advanceHistoryEpoch();
  const tag = `type:${id}`;
  if (tag === lastUndoTag) return; // mid-burst: keep the first (pre-burst) raw
  const node = doc.byId[id];
  const rootIndex = node.originatedFromPageHeader
    ? (pageByName(node.page)?.roots.indexOf(id) ?? -1)
    : -1;
  undoStack.push({
    kind: "raw",
    id,
    raw: prevRaw,
    page: node.page,
    context: captureHistoryContext(),
    instances: captureInstances([node.page]),
    ...(rootIndex >= 0 ? { headerRoot: { node: cloneNode(node), rootIndex } } : {}),
  });
  if (undoStack.length > 200) undoStack.shift();
  redoStack = [];
  lastUndoTag = tag;
}

/** Apply one entry and return its inverse (to push onto the opposite stack). */
function applyEntry(e: UndoEntry): UndoEntry {
  if (e.kind === "raw") {
    const node = doc.byId[e.id];
    const rootIndex = node?.originatedFromPageHeader
      ? (pageByName(node.page)?.roots.indexOf(e.id) ?? -1)
      : -1;
    const inverse: RawEntry = {
      kind: "raw",
      id: e.id,
      raw: node ? node.raw : "",
      page: e.page,
      context: captureHistoryContext(),
      instances: captureInstances([e.page]),
      ...(node && rootIndex >= 0 ? { headerRoot: { node: cloneNode(node), rootIndex } } : {}),
      ...(e.preservedIds?.length ? { preservedIds: [...e.preservedIds] } : {}),
    };
    if (node) {
      if (e.removeHeaderOnApply && node.originatedFromPageHeader) {
        setDoc(produce((s) => {
          const page = s.pages.find((p) => p.name === node.page);
          if (page) page.roots = page.roots.filter((id) => id !== e.id);
          delete s.byId[e.id];
        }));
        inverse.headerRoot = { node: cloneNode(node), rootIndex: Math.max(0, rootIndex) };
      } else {
        setDoc("byId", e.id, "raw", e.raw);
      }
      addDirty(e.page);
    } else if (e.headerRoot) {
      const restored = { ...cloneNode(e.headerRoot.node), raw: e.raw };
      setDoc(produce((s) => {
        s.byId[e.id] = restored;
        const page = s.pages.find((p) => p.name === e.page);
        if (page) page.roots.splice(Math.min(e.headerRoot!.rootIndex, page.roots.length), 0, e.id);
      }));
      inverse.headerRoot = { node: cloneNode(restored), rootIndex: e.headerRoot.rootIndex };
      inverse.removeHeaderOnApply = true;
      addDirty(e.page);
    }
    return inverse;
  }
  // Capture the CURRENT state of the same page scope as the inverse (for redo).
  const inverse = snapEntry(e.pages, e.preservedIds);
  if (e.pages === null) {
    // Whole-working-set snapshot (fallback): replace byId + pages wholesale so the
    // store is always internally consistent. (A page loaded AFTER the snapshot is
    // dropped cleanly rather than left with dangling roots — but every op that can
    // touch multiple pages now declares its scope, so this path is a last resort.)
    setDoc(
      produce((s) => {
        const nodes: Record<string, Node> = {};
        for (const id in e.nodes) nodes[id] = cloneNode(e.nodes[id]);
        s.byId = nodes;
        s.pages = e.pageObjs.map((po) => clonePages([po])[0]);
      })
    );
  } else {
    // Scoped restore: touch ONLY the affected pages, so pages loaded/edited
    // concurrently on OTHER pages are left intact.
    const scope = e.pages;
    setDoc(
      produce((s) => {
        // Drop the affected pages' CURRENT nodes (incl. ones the op added) by
        // walking their current root subtrees — O(affected page sizes), not a
        // full byId sweep. Then reinstate the snapshot. (Same root-walk
        // purgePageNodes uses for upsert/forget.)
        for (const name of scope) purgePageNodes(s, name);
        for (const id in e.nodes) s.byId[id] = cloneNode(e.nodes[id]); // reinstate the snapshot
        for (const po of e.pageObjs) {
          const restored = clonePages([po])[0];
          const i = s.pages.findIndex((p) => p.name === po.name);
          if (i >= 0) {
            // Page views key their lifetime by this object. Restore its complete
            // snapshot without unmounting open editors on ordinary undo/redo.
            const current = s.pages[i];
            for (const key of Object.keys(current)) {
              if (!Object.hasOwn(restored, key)) Reflect.deleteProperty(current, key);
            }
            Object.assign(current, restored);
          } else s.pages.push(restored);
        }
      })
    );
  }
  for (const p of e.dirty) addDirty(p);
  invalidateAllMatrixDimensions();
  return inverse;
}

export function withUndoUnit<T>(tag: string, pages: string[], fn: () => T): T {
  if (pages.some((page) => pageByName(page) && !pageWritable(page))) return undefined as T;
  if (undoSuppressionDepth > 0) return fn();

  const undoBefore = undoStack.slice();
  const redoBefore = redoStack.slice();
  const tagBefore = lastUndoTag;
  pushUndo(tag, pages);
  undoSuppressionDepth++;
  try {
    return fn();
  } catch (err) {
    undoSuppressionDepth--;
    const entry = undoStack[undoStack.length - 1];
    if (entry) applyEntry(entry);
    undoStack.length = 0;
    undoStack.push(...undoBefore);
    redoStack = redoBefore;
    lastUndoTag = tagBefore;
    throw err;
  } finally {
    if (undoSuppressionDepth > 0) undoSuppressionDepth--;
  }
}

/**
 * Is applying `e` itself a cross-page move, and if so which page GAINS?
 *
 * **H2 (K6 move census §1.3).** Undo of a move A→B *is* a cross-page move, B→A:
 * it writes N+1 files and needs the same destination-first order, the same
 * source barrier and the same recovery record as the forward move. It had none
 * of them — `applyEntry` restored both page snapshots, marked both dirty, and
 * `scheduleSave` wrote them in PARALLEL. So an undo whose gaining page was
 * refused (an external write landed there meanwhile) could still land the losing
 * page's removal, leaving the blocks in NEITHER file.
 *
 * The direction is **derived here, not recorded at `pushUndo` time**, for two
 * reasons: a future structural command cannot forget to declare something it
 * never has to declare, and redo gets identical treatment from this same code
 * rather than a mirrored copy. A root whose snapshot page differs from the page
 * it occupies right now is moving back — its snapshot page gains, its current
 * page loses.
 *
 * Called BEFORE `applyEntry`, because afterwards the document no longer shows
 * where the roots came from.
 */
function crossPageRestore(e: UndoEntry): { destinationPage: string; sourcePages: string[] } | null {
  if (e.kind !== "snap" || !e.pages || e.pages.length < 2) return null;
  const scope = new Set(e.pages);
  const gaining = new Set<string>();
  const losing = new Set<string>();
  for (const id in e.nodes) {
    const to = e.nodes[id].page;
    const from = doc.byId[id]?.page;
    if (from === undefined || from === to) continue;
    if (!scope.has(to) || !scope.has(from)) continue;
    gaining.add(to);
    losing.add(from);
  }
  // Exactly one destination, or this is not a shape the record can describe —
  // leave it on the ordinary save path rather than invent a destination.
  if (gaining.size !== 1) return null;
  const destinationPage = [...gaining][0];
  const sourcePages = [...losing].filter((name) => name !== destinationPage);
  return sourcePages.length ? { destinationPage, sourcePages } : null;
}

function performUndo(): void {
  endMoveSelectionBurst();
  advanceHistoryEpoch();
  const entry = popHistoryEntry(undoStack);
  if (!entry) return;
  if (!entryIsReplayable(entry)) {
    discardStaleHistory(entry);
    return;
  }
  const restoreViewport = entry.kind === "raw" ? captureRawHistoryViewport(entry.id) : undefined;
  const restore = crossPageRestore(entry);
  redoStack.push(applyEntry(entry));
  lastUndoTag = null;
  endEdit("undo");
  // A cross-page restore is a cross-page move: destination-first, behind the
  // barrier, inside the record (H2). `persistCrossPage` marks the destination
  // dirty itself and `releaseSourcesFor` reschedules the held sources once it
  // lands, so this replaces the blanket `scheduleSave` rather than joining it.
  if (restore) void persistCrossPage(restore.destinationPage, restore.sourcePages);
  else scheduleSave();
  restoreEntryContext(entry.context);
  restoreViewport?.();
  return;
}

function performRedo(): void {
  endMoveSelectionBurst();
  advanceHistoryEpoch();
  const entry = popHistoryEntry(redoStack);
  if (!entry) return;
  if (!entryIsReplayable(entry)) {
    discardStaleHistory(entry);
    return;
  }
  if (entry.preservedIds && hasLoadedIdentityCollision(entry.preservedIds)) {
    // The selected prerequisite is already popped. A later redo snapshot cannot
    // remain valid without it, including in page-only mode where the tagged
    // entry may have been selected from the middle of the global stack.
    redoStack = [];
    pushToast("Redo skipped: a block with the same id now exists", "error");
    return;
  }
  const restoreViewport = entry.kind === "raw" ? captureRawHistoryViewport(entry.id) : undefined;
  const restore = crossPageRestore(entry);
  undoStack.push(applyEntry(entry));
  lastUndoTag = null;
  endEdit("redo");
  // Redo of a cross-page move is the same shape as undo of one (H2).
  if (restore) void persistCrossPage(restore.destinationPage, restore.sourcePages);
  else scheduleSave();
  restoreEntryContext(entry.context);
  restoreViewport?.();
  return;
}

export function undo() {
  performUndo();
}

export function redo() {
  performRedo();
}

/** Data replay and opposite-stack insertion are complete before this function is
 * reached. Each UI step is isolated and best-effort, so a missing pane, route,
 * sidebar surface, or block cannot undo/reorder the already-applied inverse.
 * OG's restore order and global-mode app-state gate are at
 * `src/main/frontend/handler/history.cljs:10-60` (OG commit 6e7afa8eb). */
function restoreEntryContext(context: HistoryContext) {
  if (!pageOnlyHistoryMode) {
    if (context.route) {
      try {
        historyRouteContextAdapter.restore(context.route);
      } catch {
        // Route restoration is best-effort; content replay has already completed.
      }
    }
    try {
      restoreHistorySidebarContext(context.sidebar);
    } catch {
      // Sidebar restoration is best-effort; content replay has already completed.
    }
  }
  if (context.editor) {
    try {
      const node = doc.byId[context.editor.blockId];
      restoreHistoryEditorContext(context.editor, node ? node.raw.length : null);
    } catch {
      // Focus/caret restoration is best-effort; content replay has already completed.
    }
  }
}
export { beginOrContinueMoveSelectionUndo, cloneNode, clonePages, endMoveSelectionBurst, pushRawUndo, pushUndo };
