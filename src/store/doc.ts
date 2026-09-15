import type { Format, PageKind } from "../types";
import { canForceSave, flushAll, flushPage, forceSave, isDirty, isSaving, markDirty as markDirtyInner, scheduleSave, trackAssetWrite } from "../persistence";
import { createMemo, createRoot, createSignal } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { editingId } from "../editorController";
import { graphMeta, isConflicted } from "../ui";
import { recordClipboardDirtyPageForTest } from "../clipboardWorkProbe";
import { sheetConfigFromRaw } from "../sheet/config";

// The debounced persistence engine lives in persistence.ts; re-exported here so
// the rest of the app keeps importing the save API from the store.
type StoreMutationObservation = { kind: "publication" | "dirty" | "undo-snapshot"; page?: string };
let storeMutationObserverForTest: ((observation: StoreMutationObservation) => void) | null = null;

/** Test-only observation seam for proving work shape at the actual store and
 * persistence entry points. Production leaves this unset. */
export function __setStoreMutationObserverForTest(
  observer: ((observation: StoreMutationObservation) => void) | null,
) {
  storeMutationObserverForTest = observer;
}

/** Production keeps the exact persistence function identity and call path. Vitest
 * selects the observing façade once at module initialization, never per edit. */
export const markDirty: typeof markDirtyInner = import.meta.env.MODE === "test"
  ? ((...args: Parameters<typeof markDirtyInner>) => {
      storeMutationObserverForTest?.({ kind: "dirty", page: args[0] });
      recordClipboardDirtyPageForTest(args[0]);
      return markDirtyInner(...args);
    }) as typeof markDirtyInner
  : markDirtyInner;

export {
  isDirty,
  isSaving,
  scheduleSave,
  flushPage,
  flushAll,
  forceSave,
  canForceSave,
  trackAssetWrite,
};

export interface Node {
  id: string;
  raw: string;
  collapsed: boolean;
  parent: string | null; // null = a root of its page
  page: string; // owning page name
  children: string[];
  /** Frontend-only editing provenance for an existing unbulleted Markdown page
   * header. Spread-based undo snapshots retain it; DTO serialization consumes
   * it and never sends it over the wire. */
  originatedFromPageHeader?: boolean;
}

export interface FeedPage {
  name: string;
  kind: PageKind;
  title: string;
  preBlock: string | null;
  roots: string[];
  /** On-disk format (drives org vs markdown inline rendering). */
  format: Format;
  /** True for a source page Tine can't round-trip — shown but not editable. */
  readOnly: boolean;
  /** Bundled in-app Guide page: read-only and ephemeral. */
  guide: boolean;
  /** Graph-root-relative file this page was loaded from. Sent back on save so a
   *  page pinned to a SPECIFIC file (a duplicate-day stray, #21) saves to its own
   *  file, not the canonical one. Empty/absent for a brand-new page (resolved by
   *  name). */
  path?: string;
}

interface DocState {
  byId: Record<string, Node>;
  // The working set: every page currently loaded in the frontend — the main
  // view's pages PLUS any page a satellite surface (sidebar, query result,
  // embed) has pulled in on demand. All share one `byId` keyed by stable block
  // uuid, so a block rendered in two places is the SAME node and edits to it
  // propagate everywhere via SolidJS reactivity (OG's "everything is a block",
  // adapted to lazy loading — the Rust cache is the full graph DB).
  pages: FeedPage[];
  // Page names the MAIN content area shows, in order (a single page, or the
  // journals feed). A subset of `pages`.
  feed: string[];
  loaded: boolean;
}

const [doc, setDocInner] = createStore<DocState>({ byId: {}, pages: [], feed: [], loaded: false });
export { doc };

/** Production keeps Solid's original setter. Vitest chooses the observing
 * façade once at module initialization, so there is no production wrapper or
 * observer check on the editing hot path. */
export const setDoc: typeof setDocInner = import.meta.env.MODE === "test"
  ? ((...args: unknown[]) => {
      storeMutationObserverForTest?.({ kind: "publication" });
      return (setDocInner as (...innerArgs: unknown[]) => unknown)(...args);
    }) as typeof setDocInner
  : setDocInner;

export interface LoadedIdentityWorkForTest {
  loaded_identity_passes: number;
  loaded_identity_nodes_scanned: number;
  loaded_identity_raw_bytes_scanned: number;
  incoming_identity_ids_checked: number;
}

const loadedIdentityWorkForTest: LoadedIdentityWorkForTest = {
  loaded_identity_passes: 0,
  loaded_identity_nodes_scanned: 0,
  loaded_identity_raw_bytes_scanned: 0,
  incoming_identity_ids_checked: 0,
};

/** Reset the F3 identity-work receipt. Test-only branches in the collision
 * helper are dead-code-eliminated from production builds. */
export function __resetLoadedIdentityWorkForTest(): void {
  if (import.meta.env.MODE !== "test") return;
  loadedIdentityWorkForTest.loaded_identity_passes = 0;
  loadedIdentityWorkForTest.loaded_identity_nodes_scanned = 0;
  loadedIdentityWorkForTest.loaded_identity_raw_bytes_scanned = 0;
  loadedIdentityWorkForTest.incoming_identity_ids_checked = 0;
}

/** Snapshot the resettable F3 identity-work receipt for focused tests. */
export function __loadedIdentityWorkForTest(): LoadedIdentityWorkForTest {
  return { ...loadedIdentityWorkForTest };
}

// Keep the existing fail-closed coverage for a Markdown `id::` line and an Org
// `:id:` property line. In particular, do not require an Org drawer here: the
// previous safety check refused any matching loaded raw line, including one
// introduced by setRaw before the byId key can be reconciled.
const RAW_BLOCK_ID_PROPERTY_RE = /(?:^|\r?\n)[ \t]*(?:id[ \t]*::|:id:)[ \t]*([^\r\n]*?)[ \t]*(?=\r?\n|$)/gi;

/**
 * Does one candidate list collide with a live identity in the loaded document?
 *
 * The old per-id predicate re-scanned all loaded keys and raws for every
 * candidate. Preserved cut paste and redo now build the loaded identity set
 * once, then check the moving IDs in O(moved IDs). Raw properties remain part
 * of the set so a synchronous setRaw cannot open a duplicate-id window.
 */
function hasLoadedIdentityCollision(incomingIds: readonly string[]): boolean {
  if (import.meta.env.MODE === "test") {
    loadedIdentityWorkForTest.incoming_identity_ids_checked += incomingIds.length;
  }

  // Reject an internally duplicated moved payload before consulting the loaded
  // document. paste normally catches this earlier, but redo history can carry
  // preserved IDs from an older snapshot and must be equally fail-closed.
  const incoming = new Set<string>();
  for (const id of incomingIds) {
    const normalized = id.toLowerCase();
    if (incoming.has(normalized)) return true;
    incoming.add(normalized);
  }

  if (import.meta.env.MODE === "test") loadedIdentityWorkForTest.loaded_identity_passes++;
  const loaded = new Set<string>();
  for (const [key, node] of Object.entries(doc.byId)) {
    loaded.add(key.toLowerCase());
    const raw = node.raw;
    if (import.meta.env.MODE === "test") {
      loadedIdentityWorkForTest.loaded_identity_nodes_scanned++;
      loadedIdentityWorkForTest.loaded_identity_raw_bytes_scanned += new TextEncoder().encode(raw).byteLength;
    }
    RAW_BLOCK_ID_PROPERTY_RE.lastIndex = 0;
    let match: RegExpExecArray | null;
    while ((match = RAW_BLOCK_ID_PROPERTY_RE.exec(raw)) !== null) {
      const identity = match[1].trim();
      if (identity) loaded.add(identity.toLowerCase());
    }
  }
  return incomingIds.some((id) => loaded.has(id.toLowerCase()));
}

// A generation identifies one exact loaded page instance. It is deliberately
// frontend-only and monotonic across resets: a later page with the same name and
// path must never satisfy a cut payload captured from an evicted/deleted/rebound
// instance. Stage B uses this at its durable-retirement boundary.
let pageInstanceClock = 0;
const pageInstanceGenerations = new Map<string, number>();

// Advances on every CONTENT edit, which `pageInstanceGeneration` deliberately
// does not: that counter tracks page instances (install/retire), so `setRaw`
// leaves it unchanged. An authority captured at a click therefore cannot use it
// to tell "the user typed while my read was in flight" from "nothing happened" —
// which is the difference between honouring a discard and destroying text the
// user entered after asking for it. (GH #254 increment 3.)
let editClock = 0;
const editGenerations = new Map<string, number>();

// Advances whenever a component-local editor transaction starts. Unlike the
// content-edit generation above, this also covers input that intentionally has
// not reached the store yet: a title-rename draft and an active IME composition.
// A discard click captures both generations so it can authorise the state the
// user saw without authorising a new local transaction begun during an await.
let editorTransactionClock = 0;
const editorTransactionGenerations = new Map<string, number>();
const [mutationBusyPages, setMutationBusyPages] = createSignal<ReadonlySet<string>>(new Set());
const [visibleMutationBusyPages, setVisibleMutationBusyPages] = createSignal<ReadonlySet<string>>(new Set());
// Monotonic per-block count of SOURCE collapse writes (setCollapsed /
// setCollapsedDeep / setCollapsedDescendants via writeCollapsed). Surfaces
// keeping an ephemeral local fold (embeds, GH #360) compare the epoch captured
// at fold time: once it advances, the local fold is stale and the source
// reclaims authority.
const [collapseEpochState, setCollapseEpochState] = createStore<{ byId: Record<string, number> }>({ byId: {} });
export const collapseEpochOf = (id: string): number => collapseEpochState.byId[id] ?? 0;
function bumpCollapseEpoch(id: string) {
  setCollapseEpochState("byId", id, (epoch = 0) => epoch + 1);
}
function bumpCollapseEpochs(ids: readonly string[]) {
  setCollapseEpochState("byId", produce((epochs) => {
    for (const id of ids) epochs[id] = (epochs[id] ?? 0) + 1;
  }));
}

/** Current content-edit generation for a page. */
export function editGeneration(name: string): number {
  return editGenerations.get(name) ?? 0;
}

export function bumpEditGeneration(name: string): void {
  editGenerations.set(name, ++editClock);
}

/** Current component-local editor-transaction generation for a page. */
export function editorTransactionGeneration(name: string): number {
  return editorTransactionGenerations.get(name) ?? 0;
}

export function pageMutationBusy(name: string): boolean {
  return mutationBusyPages().has(name);
}

/** Whether the page should visibly present a destructive/replacement hold.
 * Persistence holds alone do not dim the feed; only explicit native mutations
 * (holdPageMutationUi) do. */
export function pageMutationVisiblyBusy(name: string): boolean {
  return visibleMutationBusyPages().has(name);
}

/** Keep the named page editors inert across one explicit native mutation.
 * This is UI ownership only; callers drain then separately hold persistence. */
export function holdPageMutationUi(pages: readonly string[]): () => void {
  const held = [...new Set(pages)];
  setMutationBusy(held, true);
  setVisibleMutationBusy(held, true);
  let released = false;
  return () => {
    if (released) return;
    released = true;
    setVisibleMutationBusy(held, false);
    setMutationBusy(held, false);
    // Releasing explicit ownership is a real replacement-gate transition, just
    // like ending an edit or draining a save. A winner-file watcher event can
    // arrive while Concord owns the page and be deferred by
    // reloadPageIfStillSafe; leaving that event parked until the user's next
    // keystroke makes the fresh edit look divergent and opens Concord again.
    // Sweep only after every held page is released; each watcher re-checks the
    // full gate synchronously before it is notified.
    sweepReplaceable();
  };
}

function setMutationBusy(pages: readonly string[], busy: boolean): void {
  const next = new Set(mutationBusyPages());
  for (const page of pages) busy ? next.add(page) : next.delete(page);
  setMutationBusyPages(next);
}

function setVisibleMutationBusy(pages: readonly string[], busy: boolean): void {
  const next = new Set(visibleMutationBusyPages());
  for (const page of pages) busy ? next.add(page) : next.delete(page);
  setVisibleMutationBusyPages(next);
}

/** The page-instance generation WITHOUT creating one for a page that has none.
 *
 *  `pageInstanceGeneration` lazily activates, so reading it as a check would mint
 *  a generation and mutate the identity cut retirement compares. */
export function peekPageInstanceGeneration(name: string): number | undefined {
  return pageInstanceGenerations.get(name);
}

function activatePageInstance(name: string): number {
  const generation = ++pageInstanceClock;
  pageInstanceGenerations.set(name, generation);
  return generation;
}

function retirePageInstance(name: string): void {
  ++pageInstanceClock;
  pageInstanceGenerations.delete(name);
  editorTransactionGenerations.delete(name);
}

/** Current exact loaded-page generation, or null when that page is absent. */
export function pageInstanceGeneration(name: string): number | null {
  if (!pageByName(name)) return null;
  // Direct setDoc page seeding is supported by model tests and small embedded
  // surfaces; lazily bind it to the same invariant as loader-created pages.
  return pageInstanceGenerations.get(name) ?? activatePageInstance(name);
}

// name → index into `doc.pages`, rebuilt only when the working set's membership
// changes (add / remove / rename / evict), NOT on a keystroke. Turns the O(pages)
// linear `find` in `pageByName`/`formatForPage`/`mainPages` — which run in the
// per-block render hot path and ~7×/page render — into an O(1) lookup. We map to
// the index (not the proxy) and read `doc.pages[idx]` live, so a property change
// (roots/preBlock/format) stays fine-grained-reactive and the index never goes
// stale: the memo re-derives whenever any page's `name` or the array length moves.
const pageIndexByName = createRoot(() =>
  createMemo(() => {
    const m = new Map<string, number>();
    doc.pages.forEach((p, i) => m.set(p.name, i));
    return m;
  })
);

/** The pages shown in the main content area, in feed order. Memoized: the O(feed)
 *  resolve runs once per structural change, not on each of its ~7 calls per render. */
export const mainPages = createRoot(() =>
  createMemo((): FeedPage[] => {
    const idx = pageIndexByName();
    return doc.feed
      .map((n) => {
        const i = idx.get(n);
        return i === undefined ? undefined : doc.pages[i];
      })
      .filter(Boolean) as FeedPage[];
  })
);

/** A loaded page record by name (anywhere in the working set), or undefined. */
export function pageByName(name: string): FeedPage | undefined {
  const i = pageIndexByName().get(name);
  return i === undefined ? undefined : doc.pages[i];
}

/** The format ("md"/"org") to parse a page's inline content with. Exact for a
 *  loaded page; for one that isn't loaded (e.g. the source of a backlink) fall back
 *  to the graph's preferred format — correct for single-format graphs, a safe guess
 *  otherwise (and far better than always assuming Markdown). Used by the inline
 *  renderers (InlineText callers) so org markup in property values / breadcrumbs /
 *  reference previews / block-refs renders as org, not literally. */
export function formatForPage(name: string | undefined): Format {
  if (name) {
    const p = pageByName(name);
    if (p?.format) return p.format;
  }
  return graphMeta()?.preferred_format ?? "md";
}

/** Like {@link formatForPage} but keyed by a block id (→ its page). */
export function formatForBlock(id: string | undefined): Format {
  return formatForPage(id ? doc.byId[id]?.page : undefined);
}

export function blockIsGridView(id: string | undefined): boolean {
  const n = id ? doc.byId[id] : undefined;
  return !!n && sheetConfigFromRaw(n.raw, formatForBlock(id)).view === "grid";
}

function blockIsOpaqueSheetView(id: string | undefined): boolean {
  const n = id ? doc.byId[id] : undefined;
  const view = n ? sheetConfigFromRaw(n.raw, formatForBlock(id)).view : null;
  return view === "grid" || view === "table" || view === "board";
}

let idCounter = 0;
function freshId(): string {
  return `b${Date.now().toString(36)}-${idCounter++}`;
}

function removeNodeSubtree(s: DocState, id: string) {
  const n = s.byId[id];
  if (!n) return;
  for (const c of n.children) removeNodeSubtree(s, c);
  delete s.byId[id];
}

/** Drop a page's blocks from the shared byId map (before replacing it). Walks the
 *  page's own root subtrees — O(page size) — rather than sweeping all of `byId`
 *  (which made loading K pages into an N-node feed O(K·N)). */
function purgePageNodes(s: DocState, pageName: string) {
  const page = s.pages.find((p) => p.name === pageName);
  if (!page) return;
  for (const r of page.roots) removeNodeSubtree(s, r);
}

export type ReloadDisposition = "reload" | "conflict" | "skip";
let isBlockMovingImpl: ((page?: string) => boolean) | null = null;

export function registerIsBlockMoving(fn: (page?: string) => boolean): void {
  isBlockMovingImpl = fn;
}

/** What to do when page `name` changed on disk (external editor / Syncthing),
 *  for the file-watcher reload sites. One rule so the (formerly 4 hand-coded)
 *  branches in Page.tsx can't diverge:
 *  - `"conflict"` — it has unsaved edits / an open conflict: surface a conflict,
 *    NEVER clobber the in-memory edit with the disk version.
 *  - `"skip"` — a block on it is being edited (don't yank the caret) or a block
 *    move is mid-flight (the textarea is transiently blurred): leave it alone.
 *  - `"reload"` — safe to replace the loaded copy with the disk version.
 *  (Both `upsertUnlessDirty` and `reloadHlsIfLoaded` now compose this with the
 *  editor-lease set via `mayReplaceInstance`; the old deliberately-weaker
 *  dirty-only guard was what GH #304 cost.) */
export function reloadDisposition(name: string): ReloadDisposition {
  // `isSaving` too: `doSave` clears `dirty` BEFORE the `await savePage`, so during the
  // save IPC the page is no longer dirty but its edit isn't durable. Reloading then
  // would clobber the in-memory edit + drop its undo, and the in-flight save would
  // conflict — silent loss (audit H1). The in-flight save's baseRev check surfaces the
  // real conflict.
  if (isDirty(name) || isConflicted(name) || isSaving(name)) return "conflict";
  const ed = editingId();
  if ((ed && doc.byId[ed]?.page === name) || isBlockMovingImpl!()) return "skip";
  return "reload";
}

/**
 * Component-local editors that currently hold uncommitted input.
 *
 * `reloadDisposition` only sees state that lives in the store, and not all
 * uncommitted user input does. The page-title rename keeps its draft in local
 * signals and an `<input>`, so replacing the page unmounts the input and the typed
 * title is gone with nothing ever having been dirty. IME composition has the same
 * shape. Enumerating those cases kept losing — each round of review found another
 * one — so a component that holds uncommitted input DECLARES itself instead.
 *
 * The registry is keyed by page, then by a unique per-component handle, because
 * one page can be mounted on several surfaces: cancelling transaction A must not
 * clear transaction B's. (GH #254 increment 3.)
 */
const editorLeases = new Map<string, Set<symbol>>();

/**
 * Take a lease for uncommitted input on `pageName`. Returns its release, which is
 * idempotent and MUST be wired to the component lifecycle (`onCleanup`), not only
 * to commit and cancel: disposing a mounted page removes the title section without
 * running either, and a literal registration would then outlive its component and
 * its draft and refuse every later replacement forever.
 */
export function takeEditorLease(pageName: string): () => void {
  const handle = Symbol("editor-lease");
  editorTransactionGenerations.set(pageName, ++editorTransactionClock);
  let leases = editorLeases.get(pageName);
  if (!leases) {
    leases = new Set();
    editorLeases.set(pageName, leases);
  }
  leases.add(handle);
  let released = false;
  return () => {
    if (released) return;
    released = true;
    const live = editorLeases.get(pageName);
    if (!live) return;
    live.delete(handle);
    if (live.size === 0) {
      editorLeases.delete(pageName);
      notifyPageBecameReplaceable(pageName);
    }
  };
}

/**
 * Watchers waiting for a specific page to become replaceable.
 *
 * Keyed BY PAGE, so liveness does not depend on my enumeration of emission sites
 * being complete — which is what kept failing. Explicit announcements make the
 * common transitions prompt; `sweepReplaceable()` is the net that re-checks every
 * watched page, so a route nobody thought to instrument delays a resume rather
 * than stranding it forever. (GH #254 increment 3.)
 */
const replaceableWatchers = new Map<string, Set<(pageName: string) => void>>();

export function onPageBecameReplaceable(
  pageName: string,
  listener: (pageName: string) => void,
): () => void {
  let set = replaceableWatchers.get(pageName);
  if (!set) {
    set = new Set();
    replaceableWatchers.set(pageName, set);
  }
  set.add(listener);
  let stopped = false;
  return () => {
    if (stopped) return;
    stopped = true;
    const live = replaceableWatchers.get(pageName);
    if (!live) return;
    live.delete(listener);
    if (live.size === 0) replaceableWatchers.delete(pageName);
  };
}

/** Announce `pageName` if it is genuinely replaceable now. */
export function notifyPageBecameReplaceable(pageName: string): void {
  const set = replaceableWatchers.get(pageName);
  if (!set || set.size === 0) return;
  if (!mayReplaceInstance(pageName)) return;
  for (const listener of [...set]) listener(pageName);
}

/** Re-check every watched page. The safety net behind the explicit sites. */
export function sweepReplaceable(): void {
  if (replaceableWatchers.size === 0) return;
  for (const name of [...replaceableWatchers.keys()]) notifyPageBecameReplaceable(name);
}

export function clearReplaceableWatchers(): void {
  replaceableWatchers.clear();
}

/** Does any component hold uncommitted input for this page? */
export function hasEditorLease(pageName: string): boolean {
  return (editorLeases.get(pageName)?.size ?? 0) > 0;
}

/** Drop every lease — graph reset and teardown. */
export function clearAllEditorLeases(): void {
  editorLeases.clear();
  editorTransactionGenerations.clear();
}

/**
 * May this page's loaded instance be REPLACED right now?
 *
 * The composed gate: the store's own disposition plus the component-local leases
 * it cannot see. Both halves are required, and both must be re-evaluated
 * synchronously at the final replacement boundary — every caller awaits a backend
 * read first, and the incumbent can become dirty, start saving, or begin an
 * uncommitted rename during that await. (GH #254 increment 3.)
 */
export function mayReplaceInstance(name: string): boolean {
  return reloadDisposition(name) === "reload"
    && !hasEditorLease(name)
    && !pageMutationBusy(name);
}

/** Store mutation boundary. UI affordances also hide on read-only pages, but
 * every write API must enforce this itself because menus/shortcuts/sheets can
 * call the store without entering the textarea. Guide pages are virtual and
 * equally non-writable. */
export function pageWritable(name: string): boolean {
  const page = pageByName(name);
  return !!page && !page.readOnly && !page.guide && !pageMutationBusy(name);
}

export function blockWritable(id: string): boolean {
  const node = doc.byId[id];
  return !!node && pageWritable(node.page);
}
export { activatePageInstance, blockIsOpaqueSheetView, bumpCollapseEpoch, bumpCollapseEpochs, freshId, hasLoadedIdentityCollision, pageInstanceGenerations, purgePageNodes, retirePageInstance, setCollapseEpochState, setMutationBusyPages, setVisibleMutationBusyPages, storeMutationObserverForTest };
export type { DocState };
