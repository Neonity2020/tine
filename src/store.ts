// The live editing tree. The frontend owns this during a session; all
// keystrokes and structural ops mutate it synchronously (zero IPC). Persistence
// is a debounced per-page save to Rust. See plan §"block editor model".
//
// Supports multiple pages at once (the journals feed): a single global `byId`
// map, each node tagged with its owning `page`, and an ordered `pages` list
// each with its own roots. A single-page route is just a feed of length one.

import type { BlockDto, PageDto, PageKind, RefGroup } from "./types";
import type { ClipboardBlock, ClipboardPayloadData, ClipboardSourcePage } from "./clipboard";
import type { EditorSelection } from "./editorController";
import type { ExportNode } from "./editor/exportText";
import type { GraphChange } from "./backend";
import type { OutlineNode } from "./editor/outline";
import { CLIPBOARD_PAYLOAD_MAX_BLOCKS, CLIPBOARD_PAYLOAD_MAX_RAW_BYTES } from "./clipboard";
import { DocState, blockIsOpaqueSheetView, blockWritable, clearAllEditorLeases, doc, formatForBlock, formatForPage, freshId, hasEditorLease, mainPages, markDirty, mayReplaceInstance, notifyPageBecameReplaceable, onPageBecameReplaceable, pageByName, pageInstanceGeneration, pageInstanceGenerations, pageWritable, purgePageNodes, registerIsBlockMoving, reloadDisposition, retirePageInstance, setCollapseEpochState, setDoc, setMutationBusyPages, setVisibleMutationBusyPages, sweepReplaceable } from "./store/doc";
import { EditorInstallOptions, InstanceRefusal, clearAllEditorActivations, ensurePageLoaded, evictIfNeeded, retireEditorFor, upsertPage } from "./store/lifecycle";
import { OPEN_MARKERS, leadingMarker } from "./markers";
import { applyMarkerTransition } from "./logbook";
import { backend } from "./backend";
import { batch, createMemo, createRoot, createSignal } from "solid-js";
import { beginOrContinueMoveSelectionUndo, clearUndoHistory, endMoveSelectionBurst, invalidateUndoForPage, pushRawUndo, pushUndo } from "./store/undo";
import { bumpDataRev, bumpPageInventoryRev, clearConflict, isConflicted, logbookWithSecondSupport, logicalOutdenting, pushToast, removeDeletedBlocksFromSidebar, removeDeletedPageFromNavigation, timetrackingEnabled, workflow } from "./ui";
import { clearHeldExternalChanges } from "./conflictPolicy";
import { clearMatrixDimensionCache, invalidateAllMatrixDimensions } from "./sheet/matrix";
import { clearPendingBlockRefStamps, existingBlockId } from "./store/blockRefs";
import { clearSeededFacets } from "./render/facets";
import { copyIncludeSubtree, copyStripCollapsed } from "./copySettings";
import { cycleMarkerSmart } from "./editor/repeat";
import { requestCrossPageMove } from "./crossPageMove";
import { editingId, endEdit, startEditing } from "./editorController";
import { flushPageToQuiescence, forgetSaveState, graphBinding, resetSaveState, tombstoneIfQuiescent, untombstone } from "./persistence";
import { graphBindingRuntime } from "./graphBindingRuntime";
import { isBuiltinHidden, isPropertiesOnly, joinProps, splitProps } from "./editor/properties";
import { isOrdered, orderListTypeFromRaw, rawWithCollapsed, rawWithInheritedOrderListType, rawWithOrderListType, registerSelectedIds } from "./store/properties";
import { journalTitle } from "./journal";
import { notifyModeReset, notifyOutlineSelectionStarted } from "./modeHooks";
import { produce } from "solid-js/store";
import { projectPageDto, toDto } from "./store/mutationPlans";
import { recordClipboardWorkForTest } from "./clipboardWorkProbe";
import { resetReferenceSectionState } from "./referenceSectionState";
import { registerInsertOutlineAfter } from "./store/paste";
export { __loadedIdentityWorkForTest, __resetLoadedIdentityWorkForTest, __setStoreMutationObserverForTest, blockIsGridView, blockWritable, bumpEditGeneration, canForceSave, clearAllEditorLeases, clearReplaceableWatchers, collapseEpochOf, doc, editGeneration, editorTransactionGeneration, flushAll, flushPage, forceSave, formatForBlock, formatForPage, hasEditorLease, holdPageMutationUi, isDirty, isSaving, mainPages, markDirty, mayReplaceInstance, notifyPageBecameReplaceable, onPageBecameReplaceable, pageByName, pageInstanceGeneration, pageMutationBusy, pageMutationVisiblyBusy, pageWritable, peekPageInstanceGeneration, reloadDisposition, scheduleSave, setDoc, sweepReplaceable, takeEditorLease, trackAssetWrite } from "./store/doc";
export type { FeedPage, LoadedIdentityWorkForTest, Node, ReloadDisposition } from "./store/doc";
export { clearAllEditorActivations, clearEditorActivation, editorActivationFor, ensurePageLoaded, focusFreshnessPageNames, registerPaneRouteProvider, retireEditorFor, setEditorActivation, setProspectiveTarget } from "./store/lifecycle";
export type { EditorInstallOptions, InstanceRefusal } from "./store/lifecycle";
export { clearUndoHistory, historyPageOnlyMode, installHistoryRouteContextAdapter, invalidateUndoForPage, redo, toggleUndoRedoMode, undo, undoTopTag, withUndoUnit } from "./store/undo";
export type { HistoryRouteContext } from "./store/undo";
export { beginPageHeaderEdit, blockPageReadOnly, blockProperty, collapsibleDescendantIds, ensurePagePropertyOnKeyPage, expandAncestors, finishPageHeaderEdit, makeOwnNumberedList, orderedListMarker, promotePagePreamble, readPageProperty, readSchedule, removeOwnNumberedList, setBlockProperty, setCollapsed, setCollapsedDeep, setCollapsedDescendants, setHeading, setPageProperty, setSchedule, setSelectionHeading, stopOwnNumberedListOnEmptyEnter, toggleBlockProperty, toggleCollapse, toggleListItem, toggleListItemAtIndex, toggleOwnNumberedList } from "./store/properties";
export type { HeadingState } from "./store/properties";
export { __pageMutationPlanDeeplyFrozenForTest, __setPageMutationEffectFailureForTest, applyPageMutationPlan, createPageMutationPlan } from "./store/mutationPlans";
export type { PageMutationAuthority, PageMutationDispatch, PageMutationDraft, PageMutationDraftNode, PageMutationDraftPage, PageMutationEffect, PageMutationPlan } from "./store/mutationPlans";
export { blockExternalId, blockRef, clearPendingBlockRefStamps, ensureBlockId, ensureStableBlockId, existingBlockId, hasPendingBlockRefStamp, persistBlockRefTarget, persistentBlockRef, rawWithBlockId, resolveBlockRef } from "./store/blockRefs";
export type { LoadedBlockRef } from "./store/blockRefs";
export { BULK_INSERTION_UNAVAILABLE_TOAST, appendToTodayJournal, bulkRouteFenceCurrent, captureBulkRouteFence, captureToPage, pasteClipboardPayload } from "./store/paste";
export type { BulkInsertionPreflight, BulkRouteFence } from "./store/paste";
export { CROSS_PAGE_MOVE_BLOCKED_TOAST, prepareCrossPageSources, settleDirectMovesForTest, withDirectMoveRecord } from "./crossPageMove";
export type { CrossPageMoveIntent, CrossPageMoveOutcome } from "./crossPageMove";


/** Install the isolated quick-capture scratch DTO without touching core.
 * It is the sole C9 exception: local-only, never graph-persisted, and therefore
 * deliberately has no editor activation. */
export function installCaptureScratchPage(dto: PageDto): void {
  upsertPage(dto);
}

/** Load a page selected by the main graph router. `ensurePageLoaded` also serves
 * the isolated quick-capture window, where `doc.loaded` must stay false so its
 * scratch editor can never write the graph. A successfully resolved main route,
 * however, is sufficient to arm ordinary persistence even when an invalidated
 * Journals reload has not landed yet. */
export async function loadRoutedPage(
  dto: PageDto,
  expectedGraphBinding = graphBinding(),
): Promise<InstanceRefusal | null> {
  const refusal = await ensurePageLoaded(dto, { expectedGraphBinding });
  if (refusal) {
    // A refused route must not leave the surface silently blank — that is a trap,
    // not a safeguard. The route currently marks itself loaded after this call and
    // its loader effect watches route/graph identity rather than the incumbent's
    // save lifecycle, so nothing would retry on its own. Say what is holding the
    // file and what resolves it, so the user can act and ask again.
    // (GH #254 increment 3.)
    const message = refusal.reason === "unsaved-changes"
      ? `“${refusal.page}” has unsaved changes, so the other file with that name can't be shown yet. ` +
        `Save or resolve it, then open the file again.`
      : refusal.reason === "activation-failed"
        ? `“${refusal.page}” could not be activated for editing. Open it again to retry.`
        : `The request for “${refusal.page}” became stale. Open it again to retry.`;
    pushToast(message, "error");
    return refusal;
  }
  setDoc("loaded", true);
  return null;
}

/** Load/reload bundled Guide pages into the working set without making them the
 *  main feed. Re-open uses this to re-derive the read-only virtual pages from
 *  the backend templates instead of trusting stale in-memory copies. */
export function loadGuidePages(dtos: PageDto[]) {
  for (const dto of dtos) {
    upsertPage({ ...dto, read_only: true, guide: true });
  }
  evictIfNeeded();
}

export function isGuidePage(name: string): boolean {
  return pageByName(name)?.guide ?? false;
}

/** Drop a page from the working set + feed and clear its dirty/baseline/conflict
 *  state — WITHOUT touching disk. Use when the page no longer exists on disk and
 *  the user accepts that (e.g. resolving an external-deletion conflict with "use
 *  disk version"): otherwise the unsaved in-memory copy is left untracked — not
 *  dirty, not conflicted — and is silently lost at close. */
export function forgetPage(name: string) {
  retireEditorFor(name);
  forgetSaveState(name);
  clearConflict(name);
  // The page is leaving the working set; a stale undo snapshot must not be able to
  // re-add it (and, with baseRev gone, recreate an externally-deleted file).
  invalidateUndoForPage(name);
  setDoc(
    produce((s) => {
      purgePageNodes(s, name);
      const pi = s.pages.findIndex((p) => p.name === name);
      if (pi >= 0) s.pages.splice(pi, 1);
      const fi = s.feed.indexOf(name);
      if (fi >= 0) s.feed.splice(fi, 1);
    })
  );
  retirePageInstance(name);
  // AFTER the dirty/conflict state is cleared and the page is gone — announcing
  // at the top ran while `mayReplaceInstance` was still false, so the
  // announcement was correctly dropped and then nothing swept again, stranding a
  // waiting request forever. This is the externally-deleted "Use disk version"
  // route and the successful `deletePage` route, which share this ordering.
  // Swept, not named: the page no longer exists, and other watchers may have been
  // freed by the same teardown. (GH #254 increment 3.)
  sweepReplaceable();
  invalidateAllMatrixDimensions();
}

export type PageDeleteLifecycle = {
  phase(name: "dirty-flush-start" | "dirty-flush-complete" | "native-command-start" | "durable-response"): void;
  /** Runs synchronously after native durability and before the deleted page is
   * removed from the working set. It must perform UI retirement only. */
  retireDurableRoute(): void;
};

/** Delete a page: tombstone it (so any pending/in-flight save can't recreate the
 *  file), drop its dirty/baseline/conflict state, remove it from the working set
 *  and feed, then delete on disk. Routing deletion through the store — rather than
 *  calling the backend directly — is what prevents a queued baseRev=null save from
 *  resurrecting a just-typed, never-saved page. Returns backend success. */
export async function deletePage(
  name: string,
  kind: PageKind,
  expectedPath?: string,
  lifecycle?: PageDeleteLifecycle,
): Promise<boolean> {
  const loaded = pageByName(name);
  if (expectedPath && loaded?.path !== expectedPath) return false;
  if (loaded?.readOnly || loaded?.guide) return false;
  // Capture the exact loaded instance before awaiting.  A replacement, graph
  // reload, or path rebind must not let this delete tombstone a later editor that
  // happens to reuse its logical name.
  const captured = loaded && {
    name: loaded.name,
    kind: loaded.kind,
    path: loaded.path,
    generation: pageInstanceGeneration(name),
  };
  // A by-name delete may target an unloaded page. With no live instance or draft
  // to protect, it can publish its name-wide tombstone synchronously below.
  if (captured && (captured.kind !== kind || captured.generation === null)) return false;
  const capturedConflicted = !!captured && isConflicted(name);
  const stillCaptured = () => {
    if (!captured) return false;
    const current = pageByName(name);
    return !!current
      && current.name === captured.name
      && current.kind === captured.kind
      && current.path === captured.path
      && pageInstanceGeneration(name) === captured.generation;
  };

  // A conflicted draft is deliberately not flushed: its current actor winner,
  // not unrecoverable draft bytes, is what the warning says reaches trash.  For
  // every other page, drain through quiescence rather than one save so a keystroke
  // injected during that first save either becomes a second accepted snapshot or
  // causes this delete to refuse with the draft still live.
  if (captured && !capturedConflicted) {
    lifecycle?.phase("dirty-flush-start");
    if (!(await flushPageToQuiescence(name))) return false;
    lifecycle?.phase("dirty-flush-complete");
  }
  // The identity proof and persistence retirement run back-to-back without a
  // yield. tombstoneIfQuiescent re-checks dirty/saving/conflict state in the same
  // synchronous turn that publishes the marker, closing the resolved-Promise
  // handoff after flushPageToQuiescence.
  if (
    (captured && !stillCaptured())
    || !tombstoneIfQuiescent(name, capturedConflicted, expectedPath)
  ) return false;
  try {
    lifecycle?.phase("native-command-start");
    if (expectedPath) await backend().deletePage(name, kind, expectedPath);
    else await backend().deletePage(name, kind);
  } catch {
    untombstone(name); // delete failed — lift the tombstone; page + edits stay intact
    // Anything that parked itself while this page looked deleted may proceed now.
    notifyPageBecameReplaceable(name);
    return false;
  }
  lifecycle?.phase("durable-response");
  // Durability is already established, but the loaded page still exists. Retire
  // every exact pane route in this same continuation so no renderer can observe
  // the impossible middle state "current route names an already-purged page".
  // A route callback is UI-only and must not change whether durable trash counts
  // as success; local retirement therefore still completes if it unexpectedly
  // throws.
  try {
    lifecycle?.retireDurableRoute();
  } catch {
    // The store remains authoritative: a durable delete must still retire its
    // tombstone, loaded instance and navigation inventories.
  }
  forgetPage(name); // success — now drop it from the working set + feed
  removeDeletedPageFromNavigation({ name, pageKind: kind, ...(expectedPath ? { path: expectedPath } : {}) });
  // A page delete changes every live query / backlink result (the backend already
  // dropped its derived cache + bumped cache_gen in delete_page). Nudge dataRev so
  // open {{query}} panels re-run and drop the deleted page's rows — otherwise they
  // keep showing the stale cached result (only the block whose node was purged from
  // byId visibly disappears, leaving the rest of the deleted page's rows behind).
  bumpDataRev();
  bumpPageInventoryRev();
  return true;
}

/** Replace a page in the working set from a fresh DTO (e.g. resolving a conflict
 *  with the disk version, or a watcher reload). Updates the main view and any
 *  satellite that shows it, since they share `byId`. */
export async function reloadPage(
  dto: PageDto,
  options: Pick<EditorInstallOptions, "isRequestLive" | "beforeInstall"> = {},
): Promise<InstanceRefusal | null> {
  return ensurePageLoaded(dto, { ...options, bypassReplacementGate: true });
}

/** Apply a watcher-driven disk reload only if it is STILL safe at this instant.
 *
 *  `reloadDisposition` is correct, but the watcher sites read its verdict and
 *  then `await backend().getPage(...)` before acting — tens to hundreds of ms on
 *  a large graph, and a Syncthing burst fires many of these concurrently. If the
 *  user clicks into a block and types inside that window, `commit()` writes into
 *  the store synchronously; the resolved IPC then replaces the page, dropping the
 *  typed text AND its undo history, with no conflict raised and nothing written
 *  to disk. (Direct Files data-safety audit, 2026-08-09, finding 5.)
 *
 *  `upsertUnlessDirty` already re-checks at the moment of the upsert; only these
 *  watcher sites skipped it. Re-checking here rather than at each call site means
 *  a fifth site cannot reintroduce the hole. `reloadPage` itself stays a
 *  deliberate clobber — "use disk version" is an explicit user decision.
 *
 *  Returns false when the reload was declined. */
export async function reloadPageIfStillSafe(
  name: string,
  dto: PageDto,
  expectedGraphBinding = graphBinding(),
): Promise<boolean> {
  // The full gate, not `reloadDisposition` alone: component-local uncommitted
  // input (the title-rename draft, IME composition) is invisible to every store
  // predicate, and this path deliberately replaces the working instance.
  if (!mayReplaceInstance(name)) return false;
  if (reloadDisposition(name) !== "reload") return false;
  return (await ensurePageLoaded(dto, { expectedGraphBinding })) === null;
}

// --- deferred replay of skipped external reloads (Concord P1, freshness) ---
//
// `reloadDisposition` answers "skip" while a block on the changed page is being
// edited or a block move is in flight, and `mayReplaceInstance` refuses while a
// component-local editor lease holds uncommitted input. Those refusals are
// correct — but the watcher event they decline used to be DROPPED: the backend
// cache was fresh while the visible page stayed stale until some unrelated
// event happened to touch it again. Record the declined change instead and
// re-dispatch it through the original watcher handler once the page becomes
// replaceable (editing ends, the move settles, the lease is released — all of
// which already announce through `notifyPageBecameReplaceable`). Re-dispatching
// the ORIGINAL handler means the disposition is re-evaluated at replay time: a
// page that became dirty meanwhile takes the normal divergence path
// (`applyDivergenceVerdict`), never a clobbering reload. No guard is weakened —
// replay still funnels through `reloadPageIfStillSafe` and the same handler
// branches as a live event.

/** Why an external reload was deferred (diagnostics; the replay re-checks). */
type DeferredExternalReloadReason =
  | "block-edit"
  | "block-move"
  | "uncommitted-input"
  | "declined";

type DeferredExternalReload = {
  change: GraphChange;
  binding: number;
  reason: DeferredExternalReloadReason;
  stop: () => void;
};

const deferredExternalReloads = new Map<string, DeferredExternalReload>();

let externalReloadReplay: ((change: GraphChange) => void) | null = null;

/** Wired once by the watcher handler's module (`handleGraphChange`); injected
 *  because the store must not import the App module. */
export function installExternalReloadReplayHandler(
  handler: (change: GraphChange) => void,
): void {
  externalReloadReplay = handler;
}

function deferredExternalReloadReason(name: string): DeferredExternalReloadReason {
  if (isBlockMoving()) return "block-move";
  const ed = editingId();
  if (ed && doc.byId[ed]?.page === name) return "block-edit";
  if (hasEditorLease(name)) return "uncommitted-input";
  return "declined";
}

/** Record an external change the watcher handler could not apply right now, and
 *  replay it when the page becomes replaceable. Latest observation wins — the
 *  replay refetches the DTO, so only the newest change's shape (kind,
 *  created/removed) matters. */
export function deferExternalReload(change: GraphChange, binding = graphBinding()): void {
  const existing = deferredExternalReloads.get(change.name);
  if (existing) {
    existing.change = change;
    existing.binding = binding;
    existing.reason = deferredExternalReloadReason(change.name);
    return;
  }
  const stop = onPageBecameReplaceable(change.name, () => {
    const pending = deferredExternalReloads.get(change.name);
    if (!pending) return;
    pending.stop();
    deferredExternalReloads.delete(change.name);
    if (pending.binding !== graphBinding()) return;
    // Fire-and-forget like a live watcher event; if the handler declines again
    // it re-defers, so the record cannot be lost between here and there.
    externalReloadReplay?.(pending.change);
  });
  deferredExternalReloads.set(change.name, {
    change,
    binding,
    reason: deferredExternalReloadReason(change.name),
    stop,
  });
}

function clearDeferredExternalReloads(): void {
  for (const pending of deferredExternalReloads.values()) pending.stop();
  deferredExternalReloads.clear();
}

const pendingHlsRefreshes = new Map<string, () => void>();

function retryHlsRefreshWhenReplaceable(name: string, binding: number): void {
  if (pendingHlsRefreshes.has(name)) return;
  const stop = onPageBecameReplaceable(name, () => {
    stop();
    pendingHlsRefreshes.delete(name);
    if (binding !== graphBinding()) return;
    void reloadHlsIfLoaded(name);
  });
  pendingHlsRefreshes.set(name, stop);
}

function clearPendingHlsRefreshes(): void {
  for (const stop of pendingHlsRefreshes.values()) stop();
  pendingHlsRefreshes.clear();
}

/** After a PDF highlight write changed an `hls__` page on disk, refresh its
 *  loaded copy (main view or sidebar) so its content AND save baseline (baseRev)
 *  track disk — otherwise a later editor save would conflict against the highlight
 *  write. Skips a page with unsaved edits / an open conflict: the caller flushes
 *  those FIRST so they're on disk and merged in, rather than clobbered here. */
export async function reloadHlsIfLoaded(name: string): Promise<boolean> {
  if (!pageByName(name)) return false;
  // The FULL gate, and re-evaluated after the await. The old dirty-or-conflicted
  // check missed uncommitted input the store cannot see — an IME composition on
  // the notes page was reproduced being destroyed here while the store was clean
  // — and checking only before the await let the page become dirty during it,
  // since `reloadPage` is a deliberate clobber. Declining is safe: the next
  // highlight write re-drives this. (GH #254 increment 3.)
  const binding = graphBinding();
  if (!mayReplaceInstance(name)) {
    retryHlsRefreshWhenReplaceable(name, binding);
    return false;
  }
  const dto = await backend().getPage(name, "page");
  if (!dto || binding !== graphBinding()) return false;
  const refusal = await ensurePageLoaded(dto, { expectedGraphBinding: binding });
  if (refusal) {
    if (refusal.reason === "unsaved-changes") retryHlsRefreshWhenReplaceable(name, binding);
    return false;
  }
  return true;
}

/** Clear the entire working set. Used for test isolation and when switching
 *  graphs; normal navigation is additive (keeps satellite pages alive). Also
 *  cancels pending saves and clears dirty flags so nothing from the old graph
 *  can be written after a switch. */
export function resetStore() {
  // Pure node tests historically exercise the synchronous Direct store without
  // opening a graph. Seed the binding once; tests that exercise the binding
  // boundary rebind explicitly after reset.
  if (import.meta.env.MODE === "test" && graphBindingRuntime.snapshot().bindingGeneration === null) {
    graphBindingRuntime.bind(1, { binding_generation: 1 });
  }
  // Every identity belongs to the graph being left. The core drops its own
  // registry with the Graph, so clearing locally is sufficient and avoids a
  // storm of per-page retirements against a graph that is going away.
  clearAllEditorActivations();
  clearAllEditorLeases();
  setMutationBusyPages(new Set<string>());
  setVisibleMutationBusyPages(new Set<string>());
  setCollapseEpochState("byId", {});
  clearDeferredExternalReloads();
  clearHeldExternalChanges(); // Concord P5: a held change belongs to its graph
  clearPendingHlsRefreshes();
  clearPendingBlockRefStamps();
  // Cancel pending/in-flight saves and clear all save guard state (timers, graph
  // token, dirty/baseline/tombstone) so nothing from the old graph can be written
  // after the switch.
  resetSaveState();
  // Drop undo/redo history: it holds page snapshots from the OLD graph; an undo
  // after a graph switch would otherwise restore (and save) those into the new
  // graph, even creating a foreign page there.
  clearUndoHistory();
  // Drop the old graph's seeded facets (the never-evicted tier) so they don't linger
  // across the switch (audit P2).
  clearSeededFacets();
  clearMatrixDimensionCache();
  // Linked/Unlinked References expand state is keyed by page identity, and every
  // page identity is retired with the old graph (GH #272).
  resetReferenceSectionState();
  for (const name of pageInstanceGenerations.keys()) retirePageInstance(name);
  setDoc({ byId: {}, pages: [], feed: [], loaded: false });
  endEdit("graph-switch");
  notifyModeReset();
}

// A navigation/feed load must NOT replace a page that has unsaved edits (or an
// unresolved conflict) with a fresh disk DTO — e.g. you edited it in the sidebar,
// then opened it in the main view before the debounce saved. Keep the live dirty
// nodes; the disk version would otherwise be served and the next save could write
// it, silently dropping the edit. (reloadPage / "use disk version" still replace
// explicitly via upsertPage.)
/** Install `dto` unless the loaded page holds unsaved work. Reports whether it
 *  actually installed, so publication can follow installation rather than assume
 *  it. (GH #254 increment 3.) */
async function upsertUnlessDirty(dto: PageDto, expectedGraphBinding: number): Promise<boolean> {
  return (await ensurePageLoaded(dto, { expectedGraphBinding })) === null;
}

/** Load a single page and make it the main view. */
export function loadSingle(dto: PageDto, opts: { endEdit?: boolean } = {}) {
  // Legacy synchronous store seeding used by isolated/test surfaces. Production
  // routed editors use `loadRoutedPage`, and feed editors use `loadFeed`; both go
  // through activation before publication. A page seeded here still acquires its
  // activation at the save boundary before any write.
  if (pageByName(dto.name) && !mayReplaceInstance(dto.name)) return false;
  upsertPage(dto);
  setDoc("feed", [dto.name]);
  setDoc("loaded", true);
  if (opts.endEdit !== false) endEdit("page-navigation");
  evictIfNeeded();
  return true;
}

/** Load the journals feed as the main view. */
export async function loadFeed(
  dtos: PageDto[],
  opts: { endEdit?: boolean; expectedGraphBinding?: number; preserveExisting?: boolean; isRequestLive?: () => boolean } = {},
): Promise<boolean> {
  // Publication FOLLOWS installation. When the DTO is declined the name used to
  // be published into the feed anyway, so the feed rendered a dirty path-pinned
  // stray as though it were the requested canonical journal — no refusal, no
  // path warning, and an edit saved to the wrong file. A page already present
  // under that name stays published; one that never installed does not.
  // (GH #254 increment 3.)
  // Publication follows INSTALLATION. An earlier draft fell back to
  // `|| pageByName(d.name)`, which reintroduced the exact defect: a dirty
  // path-pinned stray already occupying the name made the declined canonical DTO
  // publish anyway, so the feed rendered the stray as though it were the
  // requested journal.
  const binding = opts.expectedGraphBinding ?? graphBinding();
  const installed: string[] = [];
  for (const dto of dtos) {
    // Calendar rollover can add days without replacing or unmounting any live
    // feed page. In particular, even a clean active editor owns its exact nodes.
    if (opts.preserveExisting && doc.feed.includes(dto.name)) {
      installed.push(dto.name);
      continue;
    }
    if (await ensurePageLoaded(dto, { expectedGraphBinding: binding, isRequestLive: opts.isRequestLive })) return false;
    installed.push(dto.name);
  }
  if (binding !== graphBinding() || opts.isRequestLive?.() === false) return false;
  setDoc("feed", opts.preserveExisting
    ? [...installed, ...doc.feed.filter((name) => !installed.includes(name))]
    : installed);
  setDoc("loaded", true);
  if (opts.endEdit !== false) endEdit("page-navigation");
  evictIfNeeded();
  return true;
}

/** Append more pages to the journals feed (infinite scroll). */
export async function appendFeed(
  dtos: PageDto[],
  expectedGraphBinding = graphBinding(),
) {
  const binding = expectedGraphBinding;
  for (const d of dtos) {
    if (doc.feed.includes(d.name)) continue;
    // Publication follows installation — see `loadFeed`.
    if (!(await upsertUnlessDirty(d, binding))) continue;
    if (binding !== graphBinding()) return;
    setDoc("feed", [...doc.feed, d.name]);
  }
  evictIfNeeded();
}

/** A fresh, empty (unsaved) page: one editable blank block. Used for a page that
 *  doesn't exist on disk yet — the file is written lazily on first save. Shared by
 *  the feed loader (today's placeholder), single-page open, and the post-delete
 *  today restore, so the empty-page shape has ONE definition. */
export function emptyPage(name: string, kind: "journal" | "page"): PageDto {
  return {
    name,
    kind,
    title: name,
    pre_block: null,
    blocks: [{ id: `new-${name}`, raw: "", collapsed: false, children: [] }],
  };
}

/** Re-assert "the journals feed always shows today" on the LIVE feed after today's
 *  journal is deleted from it. The feed loader's `withToday` only runs on (re)load,
 *  so deleting today in place while viewing the feed would otherwise leave the top
 *  blank until you navigate away and back (#17). No-op if today is still in the feed
 *  (e.g. it was an OLDER day that got deleted). The placeholder is empty and
 *  writable — `upsertPage` lifts the delete tombstone, so the first keystroke saves
 *  a fresh file, exactly like reopening the journal. */
export async function restoreTodayJournalInFeed(): Promise<boolean> {
  const title = journalTitle(new Date());
  if (doc.feed.includes(title)) return true;
  const binding = graphBinding();
  if (!(await upsertUnlessDirty(emptyPage(title, "journal"), binding))) return false;
  if (binding !== graphBinding()) return false;
  setDoc("feed", [title, ...doc.feed]);
  return true;
}

export function pageToDto(pageName: string): PageDto | null {
  return projectPageDto(doc.pages.find((x) => x.name === pageName), doc.byId, true);
}

// ---------------------------------------------------------------------------
// Virtual-guide resolution
//
// The in-app Guide is virtual — its pages live only in this store, never on
// disk — so the backend `((uuid))` / `{{embed [[page]]}}` resolvers (which scan
// the on-disk graph) can't see them. These fall back to the LOADED guide pages
// and are consulted ONLY on a backend miss, so a real-graph ref/embed always
// prefers the disk resolver and these never shadow it.
// ---------------------------------------------------------------------------

/** The block id (`id:: <uuid>` trailer) a guide node exposes to `((uuid))`
 *  references — matching the backend, which keys a block by its persisted id::. */
function guideBlockDurableId(raw: string): string | null {
  const m = /(?:^|\n)id:: *(\S+)/i.exec(raw);
  return m ? m[1] : null;
}

function findGuideNode(ids: string[], uuid: string): string | null {
  for (const id of ids) {
    const n = doc.byId[id];
    if (!n) continue;
    if (id === uuid || guideBlockDurableId(n.raw) === uuid) return id;
    const child = findGuideNode(n.children, uuid);
    if (child) return child;
  }
  return null;
}

/** Resolve a `((uuid))` block reference / block embed against the loaded guide
 *  pages. Returns null for any id not owned by a loaded guide page, so real
 *  refs fall through to the backend/disk resolver unchanged. */
export function resolveGuideBlockRef(uuid: string): RefGroup | null {
  for (const p of doc.pages) {
    if (!p.guide) continue;
    const hit = findGuideNode(p.roots, uuid);
    if (hit) return { page: p.name, kind: p.kind, blocks: [toDto(hit)] };
  }
  return null;
}

/** Serialize a loaded guide page (matched by its bare title, e.g.
 *  "Features/Tips & shortcuts") to a PageDto for in-app `{{embed [[page]]}}` —
 *  the embed macro carries no source context to remap the name, so we match on
 *  title. Null for non-guide/unloaded titles → the backend/disk path wins. */
export function resolveGuidePageDto(title: string): PageDto | null {
  const p = doc.pages.find((x) => x.guide && x.title === title);
  return p ? pageToDto(p.name) : null;
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

function rootsOf(id: string): string[] {
  const n = doc.byId[id];
  if (n.parent !== null) return doc.byId[n.parent].children;
  const p = doc.pages.find((x) => x.name === n.page);
  return p ? p.roots : [];
}

function indexInSiblings(id: string): number {
  return rootsOf(id).indexOf(id);
}

/** Visible blocks in the MAIN view, in display order (drives editor arrow-nav),
 *  plus an id→index map. Memoized: it's recomputed only when the feed or a
 *  collapsed/children state changes (NOT on plain typing), and shared across the
 *  many callers in one tick. Scoped to the feed so navigation stays within the
 *  main content area, not satellite pages loaded for the sidebar/queries. */
const visibleData = createRoot(() =>
  createMemo(() => {
    const order: string[] = [];
    const index = new Map<string, number>();
    const walk = (ids: string[]) => {
      for (const id of ids) {
        index.set(id, order.length);
        order.push(id);
        const n = doc.byId[id];
        if (n && !n.collapsed && n.children.length && !blockIsOpaqueSheetView(id)) walk(n.children);
      }
    };
    for (const p of mainPages()) walk(p.roots);
    return { order, index };
  })
);
export function visibleOrder(): string[] {
  return visibleData().order;
}

// Visible (expanded) block order within a single page — the fallback for blocks
// that aren't part of the main routed view, e.g. the quick-capture scratch page,
// whose roots never appear in mainPages(). Without this, prevVisible/nextVisible
// (and therefore Backspace-merge and Up/Down nav) are dead in the capture window.
export function pageVisibleOrder(pageName: string): string[] {
  const order: string[] = [];
  const page = doc.pages.find((p) => p.name === pageName);
  if (!page) return order;
  const walk = (ids: string[]) => {
    for (const id of ids) {
      order.push(id);
      const n = doc.byId[id];
      if (n && !n.collapsed && n.children.length && !blockIsOpaqueSheetView(id)) walk(n.children);
    }
  };
  walk(page.roots);
  return order;
}

/** Model-only description of the outline currently rendered around a block.
 * Zoom uses a single root whose durable collapse is overridden for this view.
 *
 * Reference/query/embed groups render an ARBITRARY display list of roots
 * (backlink hits, query results) that is not the outline. Such a scope is
 * `navOnly`: arrow navigation and view-local selection read it, but
 * structural mutations (merges/indents/moves) must NOT treat the display list
 * as the outline — they fall back to page order instead (GH #341). */
export interface OutlineScope {
  roots: string[];
  forceExpandedRoot?: string;
  /** A secondary surface's collapse contract (e.g. LiveRefGroup's local
   * collapse), so the scoped visible order mirrors what is actually rendered
   * rather than the durable `node.collapsed` flags. */
  collapsed?: (id: string, stored: boolean) => boolean;
  navOnly?: boolean;
}

function scopedVisibleOrder(scope: OutlineScope): string[] {
  const order: string[] = [];
  const walk = (ids: readonly string[]) => {
    for (const id of ids) {
      const node = doc.byId[id];
      if (!node) continue;
      order.push(id);
      const collapsed = scope.collapsed?.(id, node.collapsed) ?? node.collapsed;
      const expanded = !collapsed || id === scope.forceExpandedRoot;
      if (expanded && node.children.length && !blockIsOpaqueSheetView(id)) walk(node.children);
    }
  };
  walk(scope.roots);
  return order;
}

/** The only trailing-block reuse candidate for a rendered outline boundary.
 * The caller must supply the actual page or zoom scope so journal days cannot
 * cross-select each other. A collapsed parent and an opaque Sheet host remain
 * visible terminal rows, but their storage children mean neither is a leaf. */
export function trailingVisibleEmptyLeaf(scope: OutlineScope): string | null {
  const id = scopedVisibleOrder(scope).at(-1);
  if (!id) return null;
  const node = doc.byId[id];
  if (!node || node.children.length !== 0) return null;
  return splitProps(node.raw, isBuiltinHidden, formatForBlock(id)).visible.trim() === "" ? id : null;
}

let activeSelectionScope: OutlineScope | null = null;

/** Visible order to resolve a block SELECTION against. The journals feed lives in
 *  visibleData(); a routed single page is loaded via ensurePageLoaded and is NOT in
 *  doc.feed, so its blocks aren't in visibleOrder() — fall back to that block's own
 *  page order, mirroring prevVisible/nextVisible. Without this, block-select (Esc,
 *  Arrow, Shift+Arrow) is dead on any routed page / reference / embed. */
function selectionOrder(id: string | null, scope: OutlineScope | null = activeSelectionScope): string[] {
  if (!id) return [];
  if (scope) return scopedVisibleOrder(scope);
  if (visibleData().index.has(id)) return visibleOrder();
  const page = doc.byId[id]?.page;
  return page ? pageVisibleOrder(page) : [];
}

export function prevVisible(id: string, scope: OutlineScope | null = null): string | null {
  if (scope) {
    const order = scopedVisibleOrder(scope);
    const i = order.indexOf(id);
    return i > 0 ? order[i - 1] : null;
  }
  const { order, index } = visibleData();
  const i = index.get(id);
  if (i !== undefined) return i > 0 ? order[i - 1] : null;
  const node = doc.byId[id];
  if (!node) return null;
  const ord = pageVisibleOrder(node.page);
  const j = ord.indexOf(id);
  return j > 0 ? ord[j - 1] : null;
}

export function nextVisible(id: string, scope: OutlineScope | null = null): string | null {
  if (scope) {
    const order = scopedVisibleOrder(scope);
    const i = order.indexOf(id);
    return i >= 0 && i < order.length - 1 ? order[i + 1] : null;
  }
  const { order, index } = visibleData();
  const i = index.get(id);
  if (i !== undefined) return i < order.length - 1 ? order[i + 1] : null;
  const node = doc.byId[id];
  if (!node) return null;
  const ord = pageVisibleOrder(node.page);
  const j = ord.indexOf(id);
  return j >= 0 && j < ord.length - 1 ? ord[j + 1] : null;
}

export function depthOf(id: string): number {
  let d = 0;
  let p = doc.byId[id]?.parent ?? null;
  while (p !== null) {
    d++;
    p = doc.byId[p].parent;
  }
  return d;
}

// ---------------------------------------------------------------------------
// Mutations (each schedules a debounced save of the affected page)
// ---------------------------------------------------------------------------

export function setRaw(id: string, raw: string, opts?: { timetracking?: boolean }) {
  if (!blockWritable(id)) return;
  const prev = doc.byId[id].raw;
  const next =
    opts?.timetracking === false
      ? raw
      : applyMarkerTransition(
          prev,
          raw,
          formatForBlock(id),
          timetrackingEnabled(),
          logbookWithSecondSupport(),
        );
  pushRawUndo(id, prev);
  setDoc("byId", id, "raw", next);
  markDirty(doc.byId[id].page);
}

export function insertEmptyChildBlock(parentId: string, at: number): string | null {
  const parent = doc.byId[parentId];
  if (!parent || !blockWritable(parentId) || at < 0 || at > parent.children.length) return null;
  pushUndo(`insert-child:${parentId}`, [parent.page]);
  const id = freshId();
  const pageName = parent.page;
  setDoc(
    produce((s) => {
      s.byId[id] = { id, raw: "", collapsed: false, parent: parentId, page: pageName, children: [] };
      s.byId[parentId].children.splice(at, 0, id);
    })
  );
  markDirty(pageName);
  return id;
}

/** Replace child ordering for existing blocks under existing parents.
 *  Callers must pass permutations of existing child ids; this helper owns the
 *  produce-level tree write so higher-level sheet code stays out of store shape. */
export function replaceChildOrders(nextByParent: Record<string, readonly string[]>): boolean {
  const parentIds = Object.keys(nextByParent);
  if (!parentIds.length) return false;
  const pages = new Set<string>();
  for (const parentId of parentIds) {
    const parent = doc.byId[parentId];
    if (!parent || !blockWritable(parentId)) return false;
    pages.add(parent.page);
    for (const childId of nextByParent[parentId]) {
      const child = doc.byId[childId];
      if (!child || child.page !== parent.page) return false;
    }
  }
  pushUndo("replace-child-orders", [...pages]);
  setDoc(
    produce((s) => {
      for (const parentId of parentIds) {
        const next = [...nextByParent[parentId]];
        s.byId[parentId].children = next;
        for (const childId of next) s.byId[childId].parent = parentId;
      }
    })
  );
  for (const pageName of pages) markDirty(pageName);
  return true;
}

/** Append parsed outline blocks as children of `parentId`.
 *  Shared by normal editor paste (via parseOutline) and sheet indented paste. */
export function insertOutlineChildren(parentId: string, nodes: OutlineNode[]): string | null {
  if (!nodes.length) return null;
  const parent = doc.byId[parentId];
  if (!parent || !blockWritable(parentId)) return null;
  const pageName = parent.page;
  let lastId: string | null = null;
  pushUndo("paste-children", [pageName]);
  const format = formatForPage(pageName);
  setDoc(
    produce((s) => {
      const create = (n: OutlineNode, par: string): string => {
        const id = freshId();
        const childIds = n.children.map((c) => create(c, id));
        s.byId[id] = {
          id,
          raw: rawWithInheritedOrderListType(n.raw, format, parentId),
          collapsed: false,
          parent: par,
          page: pageName,
          children: childIds,
        };
        return id;
      };
      const created = nodes.map((n) => create(n, parentId));
      s.byId[parentId].children.push(...created);
      lastId = created[created.length - 1] ?? null;
    })
  );
  markDirty(pageName);
  return lastId;
}

/** Enter: split the block at `offset`. Built-in `id::`/`collapsed::` props are
 *  hidden from the editor (see editor/properties splitProps): the caret offset is
 *  in visible space, and hidden props stay with the ORIGINAL block across a split. */
export function splitBlock(
  id: string,
  offset: number,
  forceChild: boolean = false,
  keepStartInScope: boolean = false,
  editingSurface: string | null = null,
) {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return;
  pushUndo("split", [node.page]);
  const fmt = formatForBlock(id);
  // The caret offset is in editor-visible space (hidden props aren't shown), so
  // split the visible text and keep the hidden props on the original block.
  const { visible, hidden } = splitProps(node.raw, isBuiltinHidden, fmt);
  // GH #361: the caret can report either side of the same source-line boundary
  // (end of line one or start of line two). In both cases that newline becomes
  // the structural block separator instead of content in either block.
  const boundaryBefore = offset < visible.length && visible[offset] === "\n";
  const boundaryAfter = offset > 0 && visible[offset - 1] === "\n";
  const splitBefore = boundaryAfter ? offset - 1 : offset;
  const splitAfter = !boundaryAfter && boundaryBefore ? offset + 1 : offset;
  const before = visible.slice(0, splitBefore);
  const after = visible.slice(splitAfter);
  const pageName = node.page;
  // Ordered-list items propagate: a block split off an ordered item is itself
  // ordered (OG inherits `:logseq.order-list-type`), toggleable per-block later.
  const ordered = isOrdered(id);
  const withOrdered = (raw: string) => rawWithOrderListType(raw, "number", fmt);
  const orderedAfter = ordered ? withOrdered(after) : after;
  const orderedEmpty = ordered ? withOrdered("") : "";

  // Caret-at-start case (blank before, content after): create a NEW EMPTY block
  // *before* the current one. The current block keeps its uuid, its content, and
  // its children — its identity never changes. This mirrors OG's
  // insert-new-block-before-block-aux! and is what keeps a block stable when it's
  // shown elsewhere (sidebar / ref / query) and you press Enter at its head.
  // Without it, the content would migrate to a fresh uuid and any external view
  // tracking the original uuid would land on the now-empty block.
  if (before.trim() === "" && after.trim() !== "") {
    const emptyId = freshId();
    setDoc(
      produce((s) => {
        // At offset zero the original block is untouched. At a later line
        // boundary, however, the blank prefix and its separator become the new
        // empty block, so the original must retain only the post-boundary text.
        if (offset > 0) s.byId[id].raw = joinProps(after, hidden, fmt);
        s.byId[emptyId] = {
          id: emptyId,
          raw: orderedEmpty,
          collapsed: false,
          parent: keepStartInScope ? id : node.parent,
          page: pageName,
          children: [],
        };
        if (keepStartInScope) {
          s.byId[id].children.unshift(emptyId);
        } else {
          const sibs = node.parent === null
            ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
            : s.byId[node.parent].children;
          sibs.splice(sibs.indexOf(id), 0, emptyId);
        }
      })
    );
    startEditing(emptyId, 0, null, editingSurface);
    markDirty(pageName);
    return;
  }

  const newId = freshId();

  setDoc(
    produce((s) => {
      s.byId[id].raw = joinProps(before, hidden, fmt);
      const hasVisibleChildren = node.children.length > 0 && !node.collapsed;
      if (hasVisibleChildren || forceChild) {
        s.byId[newId] = {
          id: newId, raw: orderedAfter, collapsed: false, parent: id, page: pageName, children: [],
        };
        s.byId[id].children.unshift(newId);
      } else {
        s.byId[newId] = {
          id: newId, raw: orderedAfter, collapsed: false, parent: node.parent, page: pageName, children: [],
        };
        const sibs = node.parent === null
          ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
          : s.byId[node.parent].children;
        sibs.splice(sibs.indexOf(id) + 1, 0, newId);
      }
    })
  );
  startEditing(newId, 0, null, editingSurface);
  markDirty(pageName);
}

// A batched reparent publishes the new caret before DOM removal. The old
// textarea can blur during that flush; own precisely this structural handoff
// so its blur cannot erase the replacement editor's intent (GH #519/#495).
function reparentEditingBlock(page: string, update: () => void): void {
  const ownsMove = !isBlockMoving();
  if (ownsMove) setBlockMoving(true, page);
  try {
    batch(update);
    markDirty(page);
  } finally {
    if (ownsMove) setBlockMoving(false);
  }
}

/** Tab: make the block the last child of its previous sibling.
 *
 * `editingSurface` names the surface the caret must stay on, exactly as the
 * split/merge operations above take it. Without it the caret leaves a block
 * embed mid-keystroke: `editing()` in Block.tsx prefers the NON-embed rendering
 * when no surface is named, so the editor remounts on the source copy of the
 * same block further down the page (GH #477). */
export function indentBlock(id: string, caretOffset: number | EditorSelection, editingSurface: string | null = null) {
  if (!blockWritable(id)) return;
  const i = indexInSiblings(id);
  if (i <= 0) return;
  pushUndo("indent", [doc.byId[id].page]);
  const sibs = rootsOf(id);
  const newParent = sibs[i - 1];
  const pageName = doc.byId[id].page;
  // Reparenting remounts the editor. Publish its selection and ownership in the
  // same reactive flush as the tree change, before the replacement can focus.
  reparentEditingBlock(pageName, () => {
    setDoc(
      produce((s) => {
        const arr = s.byId[id].parent === null
          ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
          : s.byId[s.byId[id].parent!].children;
        arr.splice(arr.indexOf(id), 1);
        s.byId[id].parent = newParent;
        s.byId[newParent].children.push(id);
        // Expand the new parent — and clear any persisted collapsed:: in its raw,
        // else a reload would re-collapse it and hide the just-indented child.
        const np = s.byId[newParent];
        np.raw = rawWithCollapsed(np.raw, false, formatForBlock(newParent));
        np.collapsed = false;
      })
    );
    startEditing(id, caretOffset, null, editingSurface);
  });
}

/** Shift+Tab: move the block out to be the next sibling of its parent.
 *  `editingSurface` as in `indentBlock` (GH #477). */
export function outdentBlock(id: string, caretOffset: number | EditorSelection, editingSurface: string | null = null) {
  const node = doc.byId[id];
  if (!node || !blockWritable(id) || node.parent === null) return;
  pushUndo("outdent", [node.page]);
  const parentId = node.parent;
  const grandParent = doc.byId[parentId].parent;
  const pageName = node.page;

  reparentEditingBlock(pageName, () => {
    setDoc(
      produce((s) => {
        const parent = s.byId[parentId];
        const idx = parent.children.indexOf(id);
        // OG only reparents the following siblings for traditional outdenting;
        // logical outdenting stops after moving this block (`src/main/frontend/modules/outliner/core.cljs:835-852`
        // at `6e7afa8eb`). Keep this decision inside the shared store operation so
        // keyboard, mobile, and any future caller all use the same mode.
        if (logicalOutdenting()) {
          parent.children.splice(idx, 1);
        } else {
          const following = parent.children.splice(idx);
          following.shift(); // drop id
          for (const f of following) s.byId[f].parent = id;
          s.byId[id].children.push(...following);
        }
        s.byId[id].parent = grandParent;
        const gArr = grandParent === null
          ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
          : s.byId[grandParent].children;
        gArr.splice(gArr.indexOf(parentId) + 1, 0, id);
      })
    );
    startEditing(id, caretOffset, null, editingSurface);
  });
}

/** Backspace at offset 0: merge into the previous visible block (same page). */
export function mergeWithPrev(
  id: string,
  scope: OutlineScope | null = null,
  editingSurface: string | null = null,
): boolean {
  if (!blockWritable(id)) return false;
  // A navOnly display-list scope (ref/query/embed group) is never a merge
  // topology: merging into a rendered neighbor could weld unrelated subtrees
  // that merely sit adjacent in the RESULT list. Fall back to page order.
  if (scope?.navOnly) scope = null;
  const prev = prevVisible(id, scope);
  if (prev === null) return false;
  const node = doc.byId[id];
  if (doc.byId[prev].page !== node.page) return false; // don't merge across pages
  pushUndo("merge", [node.page]);
  const fmt = formatForBlock(id); // prev is same page (checked above) → same format
  // Merge visible content only; keep the previous block's hidden props (it keeps
  // its identity) and drop the absorbed block's — otherwise the id::/collapsed::
  // lines would be concatenated mid-line and a block could end up with two ids.
  const prevSplit = splitProps(doc.byId[prev].raw, isBuiltinHidden, fmt);
  const curSplit = splitProps(node.raw, isBuiltinHidden, fmt);
  const curVisible = curSplit.visible;
  const joinOffset = prevSplit.visible.length;
  const pageName = node.page;

  // Preserve the absorbed block's id if the survivor has none — otherwise inbound
  // ((id)) references to the absorbed block would orphan on merge. Match the id
  // line in the block's on-disk syntax (md `id:: x` vs org drawer `:id: x`).
  let hidden = prevSplit.hidden;
  const idPresent = fmt === "org" ? /(?:^|\n):id:\s/i : /(?:^|\n)id:: /i;
  const idLine = fmt === "org" ? /(?:^|\n)(:id:\s*\S+)/i : /(?:^|\n)(id:: \S+)/i;
  const survivorHasId = idPresent.test(prevSplit.hidden);
  const absorbedId = idLine.exec(curSplit.hidden)?.[1];
  if (!survivorHasId && absorbedId) {
    hidden = hidden ? `${hidden}\n${absorbedId}` : absorbedId;
  }

  setDoc(
    produce((s) => {
      s.byId[prev].raw = joinProps(prevSplit.visible + curVisible, hidden, fmt);
      for (const c of node.children) s.byId[c].parent = prev;
      s.byId[prev].children.push(...node.children);
      const arr = node.parent === null
        ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
        : s.byId[node.parent].children;
      arr.splice(arr.indexOf(id), 1);
      delete s.byId[id];
    })
  );
  startEditing(prev, joinOffset, null, editingSurface);
  markDirty(pageName);
  return true;
}

/** Forward-merge (Delete at the END of a block): absorb the NEXT visible block's
 *  text into the current one, so caret stays at the join point in the current
 *  block. Exact structural mirror of mergeWithPrev: visible concatenation with
 *  no separator, current block keeps its identity and hidden props, the absorbed
 *  block's id:: line attaches only if the survivor has none (inbound ((id))
 *  references to the absorbed block must not orphan), absorbed children append to
 *  the current block's, one "merge" undo snapshot, same-page only (GH #213). */
export function mergeWithNext(
  id: string,
  scope: OutlineScope | null = null,
  editingSurface: string | null = null,
): boolean {
  if (!blockWritable(id)) return false;
  // See mergeWithPrev: navOnly display lists are never a merge topology.
  if (scope?.navOnly) scope = null;
  const next = nextVisible(id, scope);
  if (next === null) return false;
  const node = doc.byId[id];
  const nextNode = doc.byId[next];
  if (nextNode.page !== node.page) return false; // don't merge across pages
  pushUndo("merge", [node.page]);
  const fmt = formatForBlock(id); // next is same page (checked above) → same format
  // Merge visible content only; keep the current block's hidden props (it keeps
  // its identity) and drop the absorbed block's hidden props.
  const curSplit = splitProps(node.raw, isBuiltinHidden, fmt);
  const nextSplit = splitProps(nextNode.raw, isBuiltinHidden, fmt);
  const nextVisibleText = nextSplit.visible;
  const joinOffset = curSplit.visible.length;
  const pageName = node.page;

  let hidden = curSplit.hidden;
  const idPresent = fmt === "org" ? /(?:^|\n):id:\s/i : /(?:^|\n)id:: /i;
  const idLine = fmt === "org" ? /(?:^|\n)(:id:\s*\S+)/i : /(?:^|\n)(id:: \S+)/i;
  const survivorHasId = idPresent.test(curSplit.hidden);
  const absorbedId = idLine.exec(nextSplit.hidden)?.[1];
  if (!survivorHasId && absorbedId) {
    hidden = hidden ? `${hidden}\n${absorbedId}` : absorbedId;
  }

  setDoc(
    produce((s) => {
      s.byId[id].raw = joinProps(curSplit.visible + nextVisibleText, hidden, fmt);
      for (const c of nextNode.children) s.byId[c].parent = id;
      s.byId[id].children.push(...nextNode.children);
      const arr = nextNode.parent === null
        ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
        : s.byId[nextNode.parent].children;
      arr.splice(arr.indexOf(next), 1);
      delete s.byId[next];
    })
  );
  startEditing(id, joinOffset, null, editingSurface);
  markDirty(pageName);
  return true;
}

/** Insert a parsed outline as siblings of `anchorId`, on the given side.
 *
 *  Returns the inserted top-level block the caller should focus: the LAST one
 *  after the anchor, the FIRST one before it — in both cases the one adjacent
 *  to the anchor is not the answer; the one the reading order ends on is. */
function insertOutlineBeside(
  anchorId: string,
  nodes: OutlineNode[],
  side: "before" | "after",
  undoLabel: string,
): string {
  if (!nodes.length) return anchorId;
  // Read-only gate at the choke point — file drops (and any future caller)
  // must not mutate a page the round-trip self-check marked read-only
  // (Phase-6 review finding, validated).
  if (!blockWritable(anchorId)) return anchorId;
  pushUndo(undoLabel, [doc.byId[anchorId].page]);
  const parent = doc.byId[anchorId].parent;
  const pageName = doc.byId[anchorId].page;
  const format = formatForPage(pageName);
  let focusId = anchorId;
  setDoc(
    produce((s) => {
      const create = (n: OutlineNode, par: string | null): string => {
        const id = freshId();
        const childIds = n.children.map((c) => create(c, id));
        s.byId[id] = {
          id,
          raw: rawWithInheritedOrderListType(n.raw, format, anchorId),
          collapsed: false,
          parent: par,
          page: pageName,
          children: childIds,
        };
        return id;
      };
      const created = nodes.map((n) => create(n, parent));
      const sibs =
        parent === null
          ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
          : s.byId[parent].children;
      sibs.splice(sibs.indexOf(anchorId) + (side === "after" ? 1 : 0), 0, ...created);
      focusId = side === "after" ? created[created.length - 1] : created[0];
    })
  );
  markDirty(pageName);
  return focusId;
}

/** Insert a parsed outline (from a paste) as siblings right after `afterId`.
 *  Returns the last top-level inserted block id (to focus). */
export function insertOutlineAfter(afterId: string, nodes: OutlineNode[]): string {
  return insertOutlineBeside(afterId, nodes, "after", "paste");
}
registerInsertOutlineAfter(insertOutlineAfter);

/** Insert a parsed outline as siblings right BEFORE `beforeId`.
 *
 *  This is the only way to put something above a block that owns its own Enter
 *  key: inside a code editor Enter inserts a newline and never splits, so the
 *  FIRST block of a page being a code block left the top of the page
 *  unreachable — there was no earlier block to insert after (GH #480).
 *  Returns the first top-level inserted block id (to focus). */
export function insertOutlineBefore(beforeId: string, nodes: OutlineNode[]): string {
  return insertOutlineBeside(beforeId, nodes, "before", "insert-block");
}

/** Replace one empty leaf with a parsed outline in one store transaction and one
 * undo entry. Structured/multiline paste uses this instead of insert-then-delete,
 * which could leave a partial import after one Undo. */
export function replaceEmptyBlockWithOutline(id: string, nodes: OutlineNode[]): string {
  const current = doc.byId[id];
  if (!nodes.length || !current || current.children.length || !blockWritable(id)) return id;
  const format = formatForBlock(id);
  const split = splitProps(current.raw, isBuiltinHidden, format);
  if (split.visible.trim()) return id;
  pushUndo("paste-replace-empty", [current.page]);
  let lastId = id;
  setDoc(produce((state) => {
    const create = (outline: OutlineNode, parent: string | null, reuseId?: string): string => {
      const created = reuseId ?? freshId();
      const children = outline.children.map((child) => create(child, created));
      const sourceRaw = reuseId ? joinProps(outline.raw, split.hidden, format) : outline.raw;
      const raw = rawWithInheritedOrderListType(sourceRaw, format, id);
      state.byId[created] = { id: created, raw, collapsed: false, parent, page: current.page, children };
      return created;
    };
    // Reuse the host for the first imported root. Besides avoiding a ghost blank,
    // this preserves its hidden id/properties and therefore inbound references.
    const created = nodes.map((node, index) => create(node, current.parent, index === 0 ? id : undefined));
    const siblings = current.parent === null
      ? state.pages[state.pages.findIndex((page) => page.name === current.page)].roots
      : state.byId[current.parent].children;
    siblings.splice(siblings.indexOf(id), 1, ...created);
    lastId = created[created.length - 1];
  }));
  markDirty(current.page);
  return lastId;
}

/** Replace a leaf slash-template trigger with its expanded outline in one
 * publication. This is deliberately narrower than the generic insertion
 * helpers: selected template admission has already accounted for reusing this
 * host, so it must not first create an over-limit temporary sibling. */
export function replaceTemplateTriggerWithOutline(id: string, nodes: OutlineNode[]): string {
  const current = doc.byId[id];
  if (!nodes.length || !current || current.children.length || !blockWritable(id)) return id;
  const format = formatForBlock(id);
  const hidden = splitProps(current.raw, isBuiltinHidden, format).hidden;
  let lastId = id;
  setDoc(produce((state) => {
    const create = (outline: OutlineNode, parent: string | null, reuseId?: string): string => {
      const created = reuseId ?? freshId();
      const children = outline.children.map((child) => create(child, created));
      const sourceRaw = reuseId ? joinProps(outline.raw, hidden, format) : outline.raw;
      state.byId[created] = {
        id: created,
        raw: rawWithInheritedOrderListType(sourceRaw, format, id),
        collapsed: false,
        parent,
        page: current.page,
        children,
      };
      return created;
    };
    const created = nodes.map((node, index) => create(node, current.parent, index === 0 ? id : undefined));
    const siblings = current.parent === null
      ? state.pages[state.pages.findIndex((page) => page.name === current.page)].roots
      : state.byId[current.parent].children;
    siblings.splice(siblings.indexOf(id), 1, ...created);
    lastId = created[created.length - 1];
  }));
  markDirty(current.page);
  return lastId;
}

/** Serialize a block (and, normally, its subtree) to Logseq markdown.
 *  - `stripId`: drop the internal `id::` property line (fence-aware) — OG does this
 *    when copying to the clipboard (`copy-to-clipboard-without-id-property!`) so a
 *    referenced block doesn't leak `id:: <uuid>` into pasted text. (Quick-capture
 *    writing to a journal FILE passes false to keep `id::`.)
 *  - `stripCollapsed`: also drop `collapsed::` (OG keeps it; opt-in cleaner copy).
 *  - `onlySelected`: when a Set is passed, recurse only into children that are in it
 *    (used by the "copy only the selected blocks, not the whole sub-tree" mode). */
export function blockSubtreeMarkdown(
  id: string,
  level = 0,
  stripId = false,
  stripCollapsed = false,
  onlySelected?: Set<string>
): string {
  const n = doc.byId[id];
  if (!n) return "";
  if (import.meta.env.MODE === "test") {
    recordClipboardWorkForTest("public_markdown_visits");
    recordClipboardWorkForTest("public_markdown_raw_bytes", new TextEncoder().encode(n.raw).byteLength);
  }
  const format = formatForBlock(id);
  const strip = stripId || stripCollapsed;
  const raw = strip
    ? splitProps(
        n.raw,
        (k) => (stripId && k === "id") || (stripCollapsed && k === "collapsed"),
        format,
      ).visible
    : n.raw;
  const lines = raw.split("\n");
  const out: string[] = [];
  // OG's clipboard path intentionally exports blocks as Markdown even when the
  // source page is Org (`export-blocks-as-markdown`), but removes IDs using the
  // SOURCE format. Keep that portable outline shape while stripping Org drawers.
  const tabs = "\t".repeat(level);
  out.push(`${tabs}- ${lines[0] ?? ""}`.replace(/\s+$/, ""));
  for (const line of lines.slice(1)) out.push(line === "" ? "" : `${tabs}  ${line}`);
  for (const c of n.children) {
    if (onlySelected && !onlySelected.has(c)) continue;
    out.push(blockSubtreeMarkdown(c, level + 1, stripId, stripCollapsed, onlySelected));
  }
  return out.join("\n");
}

/**
 * Build the private clipboard forest from selection roots. Unlike the public
 * text flavor this always includes the complete subtree and exact raw strings,
 * including id::/collapsed:: and hidden properties. Returns null (without
 * affecting the public copy) when the bounded in-memory payload is too large.
 */
export function buildClipboardPayload(ids: string[]): ClipboardPayloadData | null {
  const selected = new Set(ids.filter((id) => !!doc.byId[id]));
  const hasSelectedAncestor = (id: string): boolean => {
    let parent = doc.byId[id]?.parent ?? null;
    while (parent !== null) {
      if (selected.has(parent)) return true;
      parent = doc.byId[parent]?.parent ?? null;
    }
    return false;
  };
  const roots = [...selected].filter((id) => !hasSelectedAncestor(id));
  if (roots.length === 0) return null;

  let blockCount = 0;
  let rawBytes = 0;
  const encoder = new TextEncoder();
  const pages = new Map<string, ClipboardSourcePage>();

  const build = (id: string): ClipboardBlock | null => {
    const node = doc.byId[id];
    if (!node) return null;
    if (import.meta.env.MODE === "test") {
      recordClipboardWorkForTest("private_payload_visits");
      recordClipboardWorkForTest("private_payload_raw_bytes", new TextEncoder().encode(node.raw).byteLength);
    }
    blockCount++;
    rawBytes += encoder.encode(node.raw).byteLength;
    if (blockCount > CLIPBOARD_PAYLOAD_MAX_BLOCKS || rawBytes > CLIPBOARD_PAYLOAD_MAX_RAW_BYTES) return null;

    const page = pageByName(node.page);
    const generation = pageInstanceGeneration(node.page);
    if (!page || generation === null) return null;
    if (!pages.has(page.name)) {
      pages.set(page.name, {
        name: page.name,
        kind: page.kind,
        ...(page.path ? { path: page.path } : {}),
        generation,
      });
    }

    const children: ClipboardBlock[] = [];
    for (const child of node.children) {
      const built = build(child);
      if (!built) return null;
      children.push(built);
    }
    return { raw: node.raw, children, sourceFormat: page.format };
  };

  const blocks: ClipboardBlock[] = [];
  for (const id of roots) {
    const built = build(id);
    if (!built) return null;
    blocks.push(built);
  }
  return { blocks, sourcePages: [...pages.values()] };
}

/** Build an ExportNode forest (raw + children) for the given block ids and their
 *  subtrees — input to the configurable text exporter (Copy / Export modal). */
export function exportNodesFor(ids: string[]): ExportNode[] {
  const set = new Set(ids);
  // A multi-selection (selectedIds) is a flat slice of visible order, so it can
  // contain BOTH a parent and its descendants. Export only the selection's roots
  // — a kept node's subtree already carries its children, so emitting a selected
  // child again as a top-level node would duplicate it (the "1 2 3 1 2 3" bug).
  const hasSelectedAncestor = (id: string): boolean => {
    let p = doc.byId[id]?.parent ?? null;
    while (p !== null) {
      if (set.has(p)) return true;
      p = doc.byId[p]?.parent ?? null;
    }
    return false;
  };
  const toNode = (id: string): ExportNode | null => {
    const n = doc.byId[id];
    if (!n) return null;
    return {
      raw: n.raw,
      format: pageByName(n.page)?.format ?? "md",
      children: n.children.map(toNode).filter((x): x is ExportNode => x != null),
    };
  };
  return ids
    .filter((id) => !hasSelectedAncestor(id))
    .map(toNode)
    .filter((x): x is ExportNode => x != null);
}

/** Serialize a fetched BlockDto subtree to Logseq markdown (for pages not in the
 *  working set, e.g. copy-page-as-markdown). */
export function dtoSubtreeMarkdown(b: BlockDto, level = 0): string {
  const tabs = "\t".repeat(level);
  const lines = b.raw.split("\n");
  const out: string[] = [];
  out.push(`${tabs}- ${lines[0] ?? ""}`.replace(/\s+$/, ""));
  for (const line of lines.slice(1)) out.push(line === "" ? "" : `${tabs}  ${line}`);
  for (const c of b.children) out.push(dtoSubtreeMarkdown(c, level + 1));
  return out.join("\n");
}

/** Remove a block and its subtree. */
function deleteBlockInternal(id: string) {
  const node = doc.byId[id];
  if (!node) return;
  const pageName = node.page;
  const format = pageByName(pageName)?.format ?? "md";
  const removedSidebarIds = new Set<string>();
  const collectRemovedIds = (bid: string) => {
    const current = doc.byId[bid];
    if (!current) return;
    removedSidebarIds.add(current.id);
    const durable = existingBlockId(current.raw, format);
    if (durable) removedSidebarIds.add(durable);
    current.children.forEach(collectRemovedIds);
  };
  collectRemovedIds(id);
  setDoc(
    produce((s) => {
      const arr =
        node.parent === null
          ? s.pages[s.pages.findIndex((p) => p.name === pageName)].roots
          : s.byId[node.parent!].children;
      const ix = arr.indexOf(id);
      if (ix >= 0) arr.splice(ix, 1);
      const rm = (bid: string) => {
        for (const c of s.byId[bid].children) rm(c);
        delete s.byId[bid];
      };
      rm(id);
    })
  );
  removeDeletedBlocksFromSidebar(removedSidebarIds);
  if (editingId() === id) endEdit("delete-block");
  markDirty(pageName);
}

export function deleteBlock(id: string) {
  if (!blockWritable(id)) return;
  pushUndo("delete", [doc.byId[id].page]);
  deleteBlockInternal(id);
}

/** Re-seed the phantom empty bullet on a page emptied of its last block. Explicit
 *  "Delete block" / selection-delete bypass the Backspace last-block guard, so a page
 *  CAN reach zero roots — and then has nothing to type into. Mirrors {@link emptyPage}
 *  exactly: an editable blank root that is deliberately NOT marked dirty, so — like a
 *  brand-new day — it shows a bullet to write in but only persists to disk once the
 *  user actually types (the edit path marks it dirty then). Returns the new id, or
 *  null if the page is missing, read-only, or already non-empty. */
export function ensureEmptyBlock(pageName: string, opts: { afterProperties?: boolean } = {}): string | null {
  const page = pageByName(pageName);
  if (!page || page.readOnly) return null;
  const onlyPropertyRoot =
    opts.afterProperties === true &&
    page.format === "md" &&
    page.roots.length === 1 &&
    isPropertiesOnly(doc.byId[page.roots[0]]?.raw ?? "");
  if (page.roots.length && !onlyPropertyRoot) return null;
  const id = freshId();
  setDoc(
    produce((s) => {
      s.byId[id] = { id, raw: "", collapsed: false, parent: null, page: pageName, children: [] };
      s.pages[s.pages.findIndex((p) => p.name === pageName)].roots.push(id);
    })
  );
  return id;
}

// ---------------------------------------------------------------------------
// Multi-block selection (Escape from editing; Shift+Arrows extend) + ops
// ---------------------------------------------------------------------------

const [selAnchor, setSelAnchor] = createSignal<string | null>(null);
const [selFocus, setSelFocus] = createSignal<string | null>(null);

/** Selection endpoints are part of the move gesture identity even when their
 * normalized `topSelected()` roots happen to stay the same (for example moving
 * focus from a selected child back to its selected parent). */
function setSelectionAnchor(next: string | null): void {
  if (selAnchor() !== next) endMoveSelectionBurst();
  setSelAnchor(next);
}
function setSelectionFocus(next: string | null): void {
  if (selFocus() !== next) endMoveSelectionBurst();
  setSelFocus(next);
}

/** True when `ancestor` is a strict ancestor of `id` in the block tree. */
function isAncestorId(ancestor: string, id: string): boolean {
  let p = doc.byId[id]?.parent ?? null;
  while (p !== null) {
    if (p === ancestor) return true;
    p = doc.byId[p]?.parent ?? null;
  }
  return false;
}

/** Index of the last visible descendant of order[headIdx] within `order`.
 *  (A subtree occupies one contiguous DFS slice of the visible order.) */
function subtreeEndIndex(order: string[], headIdx: number): number {
  const head = order[headIdx];
  let end = headIdx;
  for (let k = headIdx + 1; k < order.length && isAncestorId(head, order[k]); k++) end = k;
  return end;
}

export function selectedIds(): string[] {
  const a = selAnchor();
  const f = selFocus();
  if (!a || !f) return [];
  const order = selectionOrder(a);
  const i = order.indexOf(a);
  const j = order.indexOf(f);
  if (i < 0 || j < 0) return [];
  const lo = Math.min(i, j);
  let hi = Math.max(i, j);
  // GH #262 Shift+Up: extending UP from inside a subtree onto its parent left
  // the slice [parent … anchor], omitting the parent's later children — a
  // partial-subtree selection that copy/cut/move received as "parent without
  // its children". (Shift+Down is the semantic reverse and never produces it:
  // a parent's children all follow it in visible order.) In a reverse slice,
  // any member that is an ancestor of the anchor may hold a partial subtree
  // inside the slice; complete its visible subtree.
  if (i > j) {
    for (let k = lo; k <= hi; k++) {
      if (isAncestorId(order[k], a)) {
        const end = subtreeEndIndex(order, k);
        if (end > hi) hi = end;
      }
    }
  }
  return order.slice(lo, hi + 1);
}
registerSelectedIds(selectedIds);
// Memoized set of selected ids. `isSelected` is read in the render of EVERY
// block (Block.tsx classList), and selectedIds() rebuilds visibleOrder() each
// call — so without this, a selection over N visible blocks costs O(N²). The
// memo recomputes only when the anchor/focus or the visible tree changes.
const selectedSet = createRoot(() => createMemo(() => new Set(selectedIds())));
export function isSelected(id: string): boolean {
  return selectedSet().has(id);
}
// Hierarchical Ctrl/Cmd+A (GH #262): the ancestor whose visible subtree the
// current select-all sequence covers. Any other selection mutation resets it.
let selectAllHead: string | null = null;

export function selectBlock(id: string, scope: OutlineScope | null = null) {
  endEdit("select-block");
  notifyOutlineSelectionStarted(id);
  activeSelectionScope = scope;
  selectAllHead = null;
  setSelectionAnchor(id);
  setSelectionFocus(id);
}
export function clearSelection() {
  setSelectionAnchor(null);
  setSelectionFocus(null);
  activeSelectionScope = null;
  selectAllHead = null;
}
/** Extend the current block selection's focus to `id` (mouse-drag / shift-click).
 *  Starts a fresh selection anchored at `id` if none is active. */
export function extendSelectionTo(id: string, scope: OutlineScope | null = activeSelectionScope) {
  notifyOutlineSelectionStarted(id);
  if (selAnchor() === null) {
    activeSelectionScope = scope;
    setSelectionAnchor(id);
  }
  if (activeSelectionScope && !scopedVisibleOrder(activeSelectionScope).includes(id)) return;
  selectAllHead = null;
  setSelectionFocus(id);
}
export function hasSelection(): boolean {
  return selAnchor() !== null;
}
export function moveSelection(dir: 1 | -1, extend: boolean) {
  const f = selFocus();
  if (!f) return;
  const order = selectionOrder(f);
  const i = order.indexOf(f);
  const ni = i + dir;
  if (ni < 0 || ni >= order.length) return;
  const next = order[ni];
  selectAllHead = null;
  setSelectionFocus(next);
  if (!extend) setSelectionAnchor(next);
  scrollBlockRowIntoView(next);
}

/** Ctrl/Cmd+A while editing a block, with the block's text already fully
 *  selected: escalate to a block selection covering the block's whole visible
 *  subtree, and mark it the head of the hierarchy ladder (GH #262). */
export function selectBlockSubtree(id: string, scope: OutlineScope | null = null) {
  selectBlock(id, scope);
  const order = selectionOrder(id);
  const idx = order.indexOf(id);
  if (idx < 0) return;
  setSelectionFocus(order[subtreeEndIndex(order, idx)]);
  selectAllHead = id;
}

/** Repeated Ctrl/Cmd+A in block-selection mode: widen the selection one
 *  ancestor level at a time — subtree, parent subtree, …, the whole visible
 *  outline, where it stays (idempotent) — or start the ladder at the current
 *  selection's anchor when no select-all sequence is in progress (GH #262). */
export function expandBlockSelection() {
  const a = selAnchor();
  const f = selFocus();
  if (!a || !f) return;
  const order = selectionOrder(a);
  if (order.length < 2) return;
  // Whole-outline selection is the top of the ladder; further presses no-op.
  if (a === order[0] && f === order[order.length - 1]) return;
  let head = selectAllHead;
  if (head === null || !order.includes(head)) head = a;
  let idx = order.indexOf(head);
  if (idx < 0) return;
  // A head whose subtree is fully inside the current selection (exactly or
  // because the user extended past it) is covered: climb from there.
  if (a === head && order.indexOf(f) >= subtreeEndIndex(order, idx)) {
    // This subtree is already fully selected: climb to its parent, or to the
    // whole outline when the head is already a root (within the active scope).
    const parent = doc.byId[head]?.parent ?? null;
    if (parent !== null && order.includes(parent)) {
      head = parent;
      idx = order.indexOf(head);
    } else {
      setSelectionAnchor(order[0]);
      setSelectionFocus(order[order.length - 1]);
      selectAllHead = null;
      return;
    }
  }
  setSelectionAnchor(head);
  setSelectionFocus(order[subtreeEndIndex(order, idx)]);
  selectAllHead = head;
}
/** Cycle every non-empty block in the active selection as one document
 * transaction. Each block advances from its own current marker, so a mixed
 * selection stays mixed (plain -> open, open -> active, active -> done). The
 * operation is all-or-nothing across read-only pages and preserves the visual
 * selection for repeated cycling. */
export function cycleSelectionTasks(): boolean {
  const ids = selectedIds().filter((id) => !!doc.byId[id]?.raw.trim());
  if (!ids.length || ids.some((id) => !blockWritable(id))) return false;

  const pages = [...new Set(ids.map((id) => doc.byId[id].page))];
  pushUndo("cycle-task-sel", pages);
  setDoc(
    produce((state) => {
      for (const id of ids) {
        const node = state.byId[id];
        if (!node) continue;
        // Match the existing editor command exactly: marker cycling handles
        // repeaters, while checkbox/marker-chip transitions own time tracking.
        node.raw = cycleMarkerSmart(node.raw, workflow()).raw;
      }
    })
  );
  for (const page of pages) markDirty(page);
  return true;
}

/** Keep the active end of a keyboard selection on screen: as the user holds
 *  Arrow / Shift+Arrow past the top or bottom edge, reveal the newly-focused
 *  block. Targets the block's own row (`.block-main`), not the whole `.ls-block`
 *  (which spans its children and could be taller than the viewport), and uses
 *  `block: "nearest"` so it's a no-op while the row is already visible — it only
 *  scrolls when the row crosses an edge, and never recenters mid-page. Run on the
 *  next frame so the focus class is on the DOM before we measure. */
function scrollBlockRowIntoView(id: string) {
  // No-op under the test/headless runtime (no rAF/DOM); only the real webview scrolls.
  if (typeof requestAnimationFrame !== "function" || typeof document === "undefined") return;
  requestAnimationFrame(() => {
    const sel = typeof CSS !== "undefined" && CSS.escape ? CSS.escape(id) : id;
    const row = document.querySelector(`.ls-block[data-block-id="${sel}"] > .block-main`);
    row?.scrollIntoView({ block: "nearest" });
  });
}

/** Top-level selected blocks (exclude those whose parent is also selected). */
function topSelected(): string[] {
  const ids = selectedIds();
  const set = new Set(ids);
  return ids.filter((id) => {
    const p = doc.byId[id]?.parent;
    return !(p && set.has(p));
  });
}

function selectionRemovalSurvivor(): string | null {
  const selected = selectedIds();
  const first = selected[0];
  const last = selected.at(-1);
  if (!first || !last) return null;
  return nextVisible(last) ?? prevVisible(first) ?? doc.byId[first]?.parent ?? null;
}

function reselectSurvivingBlock(id: string | null) {
  if (id && doc.byId[id]) selectBlock(id);
  else clearSelection();
}

/**
 * Confirm the assumptions that the old per-root `moveBlockInternal` loop made
 * before we start the one-shot selection mutation. A normal visible selection
 * always meets these; a stale or malformed tree instead remains a guarded
 * no-op, rather than committing only a prefix of the selection.
 */
function canBatchMoveSelectionRoots(ids: readonly string[], destPage: string, newParent: string | null): boolean {
  if (newParent !== null) {
    const parent = doc.byId[newParent];
    if (!parent || !blockWritable(newParent) || parent.page !== destPage) return false;
  }

  for (const id of ids) {
    const node = doc.byId[id];
    if (!node || !blockWritable(id) || node.page !== destPage || id === newParent) return false;
    if (!rootsOf(id).includes(id)) return false;

    // Keep the old "never make a block its own ancestor" guard, but fail
    // closed on a malformed parent cycle instead of spinning forever.
    const seen = new Set<string>();
    let cursor = newParent;
    while (cursor !== null) {
      if (cursor === id || seen.has(cursor)) return false;
      seen.add(cursor);
      const ancestor = doc.byId[cursor];
      if (!ancestor) return false;
      cursor = ancestor.parent;
    }
  }
  return true;
}

/** Remove every selected root from its actual present sibling array, then put
 * them at one destination in visible/document order. This is deliberately one
 * Solid/Immer publication: selection indent/outdent is one editor command, not
 * N independent drag operations. */
function moveSelectionRootsInOneMutation(
  ids: readonly string[],
  destPage: string,
  destinationParent: string | null,
  destinationIndex: (state: DocState) => number,
  expandParent: string | null = null,
) {
  setDoc(
    produce((state) => {
      const siblingsFor = (id: string): string[] | null => {
        const node = state.byId[id];
        if (!node) return null;
        if (node.parent === null) return state.pages.find((page) => page.name === node.page)?.roots ?? null;
        return state.byId[node.parent]?.children ?? null;
      };

      // Gather every removal before changing any array. This matters when a
      // visible selection crosses parent arrays: each root must leave its own
      // original array exactly once.
      const removals = ids.map((id) => {
        const siblings = siblingsFor(id);
        return siblings ? { siblings, index: siblings.indexOf(id) } : null;
      });
      if (removals.some((removal) => !removal || removal.index < 0)) return;

      // Re-read each index while removing: multiple selected roots can share a
      // sibling array, so their preflight indices shift after the first splice.
      // We have already established every membership above, before any mutation.
      for (const id of ids) {
        const siblings = siblingsFor(id)!;
        siblings.splice(siblings.indexOf(id), 1);
      }

      const destination = destinationParent === null
        ? state.pages.find((page) => page.name === destPage)?.roots
        : state.byId[destinationParent]?.children;
      if (!destination) return;
      const at = destinationIndex(state);
      if (at < 0) return;

      for (const id of ids) state.byId[id].parent = destinationParent;
      destination.splice(Math.min(at, destination.length), 0, ...ids);

      if (expandParent !== null) {
        const target = state.byId[expandParent];
        if (!target) return;
        // One raw rewrite plus the structure move, rather than the old
        // writeCollapsed() publication after every selected root had moved.
        target.raw = rawWithCollapsed(target.raw, false, formatForBlock(expandParent));
        target.collapsed = false;
      }
    })
  );
  markDirty(destPage);
}

export function indentSelection() {
  const ids = topSelected();
  if (!ids.length || ids.some((id) => !blockWritable(id))) return;
  const first = ids[0];
  const sibs = rootsOf(first);
  const fi = sibs.indexOf(first);
  if (fi <= 0) return;
  const newParent = sibs[fi - 1];
  if (activeSelectionScope && !scopedVisibleOrder(activeSelectionScope).includes(newParent)) return;
  // Structural indent is single-page ONLY. The target (newParent) is on first's
  // page; moving a block from another feed day under it would be a cross-page
  // structural move (removal-before-add hazard) — and indenting under a different
  // day's block is nonsensical anyway. So move only the selected blocks that are
  // already on the target page.
  const destPage = doc.byId[newParent].page;
  const same = ids.filter((id) => doc.byId[id]?.page === destPage);
  if (!same.length || !canBatchMoveSelectionRoots(same, destPage, newParent)) return;
  pushUndo("indent-sel", [destPage]);
  moveSelectionRootsInOneMutation(
    same,
    destPage,
    newParent,
    (state) => state.byId[newParent].children.length,
    newParent,
  );
}

export function outdentSelection() {
  const ids = topSelected();
  if (!ids.length || ids.some((id) => !blockWritable(id))) return;
  const parentId = doc.byId[ids[0]].parent;
  if (parentId === null) return;
  if (activeSelectionScope?.forceExpandedRoot === parentId) return;
  const grand = doc.byId[parentId].parent;
  // Single-page only (see indentSelection): outdent moves blocks to `grand`, on
  // ids[0]'s page — so restrict to the blocks already on that page.
  const destPage = doc.byId[parentId].page;
  const same = ids.filter((id) => doc.byId[id]?.page === destPage);
  if (!same.length || !canBatchMoveSelectionRoots(same, destPage, grand)) return;
  if (!rootsOf(parentId).includes(parentId)) return;
  pushUndo("outdent-sel", [destPage]);
  moveSelectionRootsInOneMutation(
    same,
    destPage,
    grand,
    (state) => {
      const siblings = grand === null
        ? state.pages.find((page) => page.name === destPage)?.roots
        : state.byId[grand]?.children;
      const parentIndex = siblings?.indexOf(parentId) ?? -1;
      return parentIndex < 0 ? -1 : parentIndex + 1;
    },
  );
}

export function deleteSelection() {
  const survivor = selectionRemovalSurvivor();
  const ids = topSelected();
  if (!ids.length || ids.some((id) => !blockWritable(id))) return;
  const pages = new Set<string>();
  for (const id of ids) {
    const n = doc.byId[id];
    if (n) pages.add(n.page);
  }
  pushUndo("delete-sel", [...pages]);
  // One produce for the whole selection — deleting each block separately fires a
  // reactive update per block (15 reflows for 15 bullets); batching collapses it
  // to a single update so the cut feels instant.
  setDoc(
    produce((s) => {
      for (const id of ids) {
        const node = s.byId[id];
        if (!node) continue;
        pages.add(node.page);
        const arr =
          node.parent === null
            ? s.pages[s.pages.findIndex((p) => p.name === node.page)].roots
            : s.byId[node.parent].children;
        const ix = arr.indexOf(id);
        if (ix >= 0) arr.splice(ix, 1);
        const rm = (bid: string) => {
          for (const c of s.byId[bid].children) rm(c);
          delete s.byId[bid];
        };
        rm(id);
      }
    })
  );
  const ed = editingId();
  if (ed && !doc.byId[ed]) endEdit("delete-selection");
  for (const p of pages) markDirty(p);
  reselectSurvivingBlock(survivor);
}

export function selectionMarkdown(): string {
  // Clipboard → always strip id:: (OG parity). collapsed:: and whole-subtree vs
  // selected-only are user-configurable (see copySettings): OG copies the full
  // sub-tree of a selected parent; Tine's default copies only the selected blocks.
  const stripCollapsed = copyStripCollapsed();
  const onlySel = copyIncludeSubtree() ? undefined : new Set(selectedIds());
  return topSelected()
    .map((id) => blockSubtreeMarkdown(id, 0, true, stripCollapsed, onlySel))
    .join("\n");
}

/** Move a block under `newParent` (or, when `newParent` is null, to the roots of
 *  `targetPage` — pass the drop target's page so a root-to-root drop across pages
 *  lands on the RIGHT page instead of defaulting back to the source). */
/** Everything a drag-move needs, computed from the CURRENT document. */
interface DragMovePlan {
  oldPage: string;
  newPage: string;
  /** The block's parent BEFORE the move, whose child list loses it. */
  parent: string | null;
  movedRaw: string;
}

export async function moveBlock(
  id: string,
  newParent: string | null,
  index: number,
  targetPage?: string,
  dropTargetId?: string,
) {
  const dragPlan = (): DragMovePlan | null => {
    const node = doc.byId[id];
    if (!node) return null;
    // Don't drop a block into its own descendant.
    let p = newParent;
    while (p !== null) {
      if (p === id) return null;
      const ancestor = doc.byId[p];
      if (!ancestor) return null;
      p = ancestor.parent;
    }
    if (newParent !== null && !doc.byId[newParent]) return null;
    const oldPage = node.page;
    // A root drop has no parent to read the page from — use the explicit target
    // page (the day/page the drop landed on); fall back to the source page only if
    // the caller didn't supply one (a same-page reorder).
    const newPage = newParent ? doc.byId[newParent].page : (targetPage ?? oldPage);
    if (!pageWritable(oldPage) || !pageWritable(newPage)) return null;
    const sourceFormat = formatForBlock(id);
    const destinationFormat = formatForPage(newPage);
    const inheritanceTarget = dropTargetId ?? newParent;
    // A cross-format move already preserves the source raw verbatim; only a newly
    // inherited property is emitted in the destination page's syntax.
    const movedRaw = orderListTypeFromRaw(node.raw, sourceFormat) !== null
      ? node.raw
      : rawWithInheritedOrderListType(node.raw, destinationFormat, inheritanceTarget);
    return { oldPage, newPage, parent: node.parent, movedRaw };
  };

  const applyDragMove = (current: DragMovePlan): boolean => {
    const { oldPage, newPage, movedRaw } = current;
    // Drag-move can cross pages → snapshot both source and destination.
    pushUndo("move", [...new Set([oldPage, newPage])]);
    setDoc(
      produce((s) => {
        const oldArr =
          current.parent === null
            ? s.pages[s.pages.findIndex((x) => x.name === oldPage)].roots
            : s.byId[current.parent].children;
        const from = oldArr.indexOf(id);
        oldArr.splice(from, 1);
        s.byId[id].parent = newParent;
        s.byId[id].raw = movedRaw;
        const newArr =
          newParent === null
            ? s.pages[s.pages.findIndex((x) => x.name === newPage)].roots
            : s.byId[newParent].children;
        let idx = index;
        if (oldArr === newArr && from < idx) idx -= 1;
        newArr.splice(Math.max(0, Math.min(idx, newArr.length)), 0, id);
        // Reassign the moved subtree to the target page.
        if (newPage !== oldPage) {
          const reassign = (bid: string) => {
            s.byId[bid].page = newPage;
            s.byId[bid].children.forEach(reassign);
          };
          reassign(id);
        }
      })
    );
    // Cross-page persistence is the front door's; a same-page reorder is one
    // ordinary save.
    if (newPage === oldPage) markDirty(oldPage);
    return true;
  };

  const first = dragPlan();
  if (!first) return;
  if (first.newPage === first.oldPage) {
    applyDragMove(first);
    return;
  }
  await requestCrossPageMove<DragMovePlan>({
    plan: dragPlan,
    intent: (current) => ({
      sourcePages: [current.oldPage],
      destinationPage: current.newPage,
      roots: [id],
    }),
    apply: applyDragMove,
  });
}

interface RelativeMovePlan {
  roots: string[];
  sourcePages: string[];
  sourcePageByRoot: string[];
  destinationPage: string;
}

/** Build the complete target-relative move plan without mutating. Captured IDs
 * are stable-deduped, then descendants of another captured ID are subsumed. */
function relativeMovePlan(capturedIds: readonly string[], targetId: string): RelativeMovePlan | null {
  const unique = [...new Set(capturedIds)];
  if (!unique.length || unique.some((id) => !doc.byId[id])) return null;
  const captured = new Set(unique);
  const roots: string[] = [];

  for (const id of unique) {
    const seen = new Set([id]);
    let parent = doc.byId[id].parent;
    let subsumed = false;
    while (parent !== null) {
      if (seen.has(parent)) return null;
      seen.add(parent);
      if (captured.has(parent)) {
        subsumed = true;
        break;
      }
      const ancestor = doc.byId[parent];
      if (!ancestor) return null;
      parent = ancestor.parent;
    }
    if (!subsumed) roots.push(id);
  }
  if (!roots.length) return null;

  const target = doc.byId[targetId];
  if (!target || !pageWritable(target.page)) return null;
  const destinationParent = target.parent;
  if (destinationParent !== null) {
    const parent = doc.byId[destinationParent];
    if (!parent || parent.page !== target.page || !blockWritable(destinationParent)) return null;
  }
  const targetSiblings = destinationParent === null
    ? pageByName(target.page)?.roots
    : doc.byId[destinationParent]?.children;
  if (!targetSiblings || targetSiblings.filter((id) => id === targetId).length !== 1) return null;

  const moved = new Set<string>();
  const visit = (id: string, page: string, ancestry: Set<string>): boolean => {
    const node = doc.byId[id];
    if (!node || node.page !== page || moved.has(id) || ancestry.has(id)) return false;
    moved.add(id);
    const childSet = new Set(node.children);
    if (childSet.size !== node.children.length) return false;
    const nextAncestry = new Set(ancestry).add(id);
    return node.children.every((childId) => {
      const child = doc.byId[childId];
      return !!child && child.parent === id && visit(childId, page, nextAncestry);
    });
  };

  const sourcePageByRoot: string[] = [];
  for (const id of roots) {
    const node = doc.byId[id];
    if (!blockWritable(id)) return null;
    const siblings = node.parent === null
      ? pageByName(node.page)?.roots
      : doc.byId[node.parent]?.children;
    if (!siblings || siblings.filter((sibling) => sibling === id).length !== 1) return null;
    if (node.parent !== null) {
      const parent = doc.byId[node.parent];
      if (!parent || parent.page !== node.page) return null;
    }
    if (!visit(id, node.page, new Set())) return null;
    sourcePageByRoot.push(node.page);
  }
  if (moved.has(targetId)) return null;

  const sourcePages = [...new Set(sourcePageByRoot)];
  if (sourcePages.some((page) => !pageWritable(page))) return null;
  return {
    roots,
    sourcePages,
    sourcePageByRoot,
    destinationPage: target.page,
  };
}

/** Move captured selection roots together before/after a live target ID. This is
 * intentionally separate from same-page selection indent/outdent and never loops
 * over moveBlock: arbitrary source sibling arrays and pages form one transaction. */
export async function moveBlocksRelative(
  capturedIds: readonly string[],
  targetId: string,
  position: "before" | "after" | "child",
): Promise<boolean> {
  let plan = relativeMovePlan(capturedIds, targetId);
  if (!plan) return false;

  const crossSources = plan.sourcePages.filter((page) => page !== plan!.destinationPage);

  /** The destination-format raw for each moved root. Pure; both arms read it. */
  const movedRawFor = (current: RelativeMovePlan): Map<string, string> => {
    const destinationFormat = formatForPage(current.destinationPage);
    return new Map(current.roots.map((id) => {
      const sourceRaw = doc.byId[id].raw;
      const raw = orderListTypeFromRaw(sourceRaw, formatForBlock(id)) !== null
        ? sourceRaw
        : rawWithInheritedOrderListType(sourceRaw, destinationFormat, targetId);
      return [id, raw];
    }));
  };

  /** The in-memory move plus Direct persistence. Unchanged from the tail this
   *  function always had; it serves the same-page route and the Direct
   *  cross-page route (which may have rebuilt `plan` first). */
  const applyRelativeMove = (): boolean => {
    const movedRaw = movedRawFor(plan!);
    const affectedPages = [...new Set([plan!.destinationPage, ...plan!.sourcePages])];
    pushUndo("move-selection-relative", affectedPages);
    setDoc(produce((state) => {
      const siblingsFor = (id: string): string[] => {
        const node = state.byId[id];
        return node.parent === null
          ? state.pages.find((page) => page.name === node.page)!.roots
          : state.byId[node.parent].children;
      };
      for (const id of plan!.roots) {
        const siblings = siblingsFor(id);
        siblings.splice(siblings.indexOf(id), 1);
      }

      const target = state.byId[targetId];
      const destinationParent = position === "child" ? targetId : target.parent;
      const destination = destinationParent === null
        ? state.pages.find((page) => page.name === target.page)!.roots
        : state.byId[destinationParent].children;
      const targetIndex = position === "child" ? destination.length : destination.indexOf(targetId);
      for (const id of plan!.roots) {
        state.byId[id].parent = destinationParent;
        state.byId[id].raw = movedRaw.get(id)!;
      }
      destination.splice(targetIndex + (position === "after" ? 1 : 0), 0, ...plan!.roots);

      const reassign = (id: string) => {
        state.byId[id].page = plan!.destinationPage;
        for (const child of state.byId[id].children) reassign(child);
      };
      for (const id of plan!.roots) reassign(id);
    }));

    // Same-page only: a cross-page move's persistence belongs to the front door.
    const persistenceSources = plan!.sourcePages.filter((page) => page !== plan!.destinationPage);
    if (!persistenceSources.length) markDirty(plan!.destinationPage);
    return true;
  };

  if (!crossSources.length) return applyRelativeMove();

  // The front door owns admission, the pre-flush, the re-plan comparison and
  // persistence; this function keeps only WHAT moves and how (I-12).
  const outcome = await requestCrossPageMove<RelativeMovePlan>({
    plan: () => relativeMovePlan(capturedIds, targetId),
    intent: (current) => ({
      sourcePages: current.sourcePages.filter((page) => page !== current.destinationPage),
      destinationPage: current.destinationPage,
      roots: current.roots,
    }),
    // A de-duplicated source LIST cannot say which root came from which page, so
    // a concurrent reparent that preserves the SET must still abort.
    unchanged: (before, after) =>
      before.sourcePageByRoot.length === after.sourcePageByRoot.length
      && before.sourcePageByRoot.every((page, index) => page === after.sourcePageByRoot[index]),
    apply: (current) => {
      plan = current;
      return applyRelativeMove();
    },
  });
  return outcome.applied;
}

/** Move a block up/down among its siblings (mod+Up/Down). Keyed <For> keeps the
 *  DOM node — so if the block is being edited, the textarea + caret survive. */
// During a block-move reorder the textarea momentarily blurs; this flag tells
// the editor's onBlur to keep edit mode (the move handler refocuses + restores
// the caret right after).
// A reorder only keeps the editor transiently blurred for one animation frame.
// Keep its page ownership: watcher/feed refreshes for another page must not be
// held hostage by a sidebar or split-pane reorder.
let blockMovingPage: string | null = null;
// Feed refresh ownership observes the end of a page-scoped drag.  Keep the
// inexpensive page check above, but make its lifecycle observable so a deferred
// restart is released by the move itself rather than a coincidental later event.
const [blockMoveRev, setBlockMoveRev] = createSignal(0);
export function isBlockMoving(page?: string): boolean {
  blockMoveRev();
  return blockMovingPage !== null && (page === undefined || blockMovingPage === page);
}
registerIsBlockMoving(isBlockMoving);
export function setBlockMoving(v: boolean, page?: string): void {
  const ended = !v && blockMovingPage !== null;
  blockMovingPage = v ? (page ?? blockMovingPage ?? "") : null;
  setBlockMoveRev((n) => n + 1);
  // A move in progress makes `reloadDisposition` return "skip", so it refuses
  // replacement exactly like a dirty page does — but unlike every other refusal
  // it announced nothing when it ended. A deferred stamp whose read landed
  // during an unrelated drag then waited for a coincidental later sweep to
  // resume, which may never come. Every state that can REFUSE has to announce
  // when it stops refusing. (GH #254 increment 3, round 13.)
  if (ended) sweepReplaceable();
}

interface OutlineStepPlan {
  parent: string | null;
  index: number;
}

/** OG's move-up/down is an outline-order operation, not merely a sibling swap.
 * At a child-list edge it crosses into the adjacent parent sibling: moving down
 * enters the next sibling as its first child; the inverse up move enters the
 * previous sibling as its last child. Root edges remain available to the
 * journal-feed cross-page route below. */
function outlineStepPlan(id: string, dir: 1 | -1): OutlineStepPlan | null {
  const node = doc.byId[id];
  if (!node) return null;
  const sibs = rootsOf(id);
  const i = sibs.indexOf(id);
  if (i < 0) return null;
  const siblingIndex = i + dir;
  if (siblingIndex >= 0 && siblingIndex < sibs.length) {
    return { parent: node.parent, index: siblingIndex };
  }
  if (node.parent === null) return null;
  const parent = doc.byId[node.parent];
  if (!parent) return null;
  const parentSiblings = rootsOf(node.parent);
  const parentIndex = parentSiblings.indexOf(node.parent);
  const adjacentParent = parentSiblings[parentIndex + dir];
  if (!adjacentParent || !doc.byId[adjacentParent]) return null;
  return {
    parent: adjacentParent,
    index: dir === -1 ? doc.byId[adjacentParent].children.length : 0,
  };
}

export function moveItem(id: string, dir: 1 | -1): boolean {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return false;
  const plan = outlineStepPlan(id, dir);
  if (!plan) return false;
  const movedRaw = plan.parent !== node.parent
    ? rawWithInheritedOrderListType(node.raw, formatForPage(node.page), plan.parent)
    : node.raw;
  pushUndo("move-item", [node.page]);
  setDoc(
    produce((s) => {
      const source =
        node.parent === null
          ? s.pages[s.pages.findIndex((p) => p.name === node.page)].roots
          : s.byId[node.parent!].children;
      const from = source.indexOf(id);
      if (from < 0) return;
      source.splice(from, 1);
      const destination = plan.parent === null
        ? s.pages[s.pages.findIndex((p) => p.name === node.page)].roots
        : s.byId[plan.parent].children;
      s.byId[id].parent = plan.parent;
      s.byId[id].raw = movedRaw;
      destination.splice(Math.max(0, Math.min(plan.index, destination.length)), 0, id);
    })
  );
  markDirty(node.page);
  return true;
}

/** Can a block move one OG outline step in `dir`? */
function canMoveItem(id: string, dir: 1 | -1): boolean {
  return outlineStepPlan(id, dir) !== null;
}

/** Selection movement is still batched as sibling-array swaps. Structural edge
 * crossing is handled separately from its root/day boundary below. */
function canMoveSelectionWithinSiblings(id: string, dir: 1 | -1): boolean {
  const sibs = rootsOf(id);
  const ni = sibs.indexOf(id) + dir;
  return ni >= 0 && ni < sibs.length;
}

// The journal feed treats its days as one continuous list: a root block at the
// top/bottom of a day moves into the adjacent *displayed* day (feed order, not
// calendar — non-displayed days like an uncreated 16th are skipped). Page.tsx
// registers a loader so a down-move past the last loaded day pulls in more.
let feedExtender: (() => Promise<boolean>) | null = null;
export function setFeedExtender(fn: (() => Promise<boolean>) | null): void {
  feedExtender = fn;
}

/** Reassign a block subtree's `page` (used when it crosses to another day). */
function reassignPage(s: DocState, id: string, page: string) {
  s.byId[id].page = page;
  for (const c of s.byId[id].children) reassignPage(s, c, page);
}

/** Move root blocks `ids` (document order) to the start (down) / end (up) of
 *  `toPage`, removing them from `fromPage`. Both pages must be loaded. */
function crossMoveBlocks(ids: string[], fromPage: string, toPage: string, dir: 1 | -1) {
  setDoc(
    produce((s) => {
      const from = s.pages.find((p) => p.name === fromPage);
      const to = s.pages.find((p) => p.name === toPage);
      if (!from || !to) return;
      const idset = new Set(ids);
      from.roots = from.roots.filter((x) => !idset.has(x));
      // up → bottom of the day above; down → top of the day below (keep order).
      if (dir === -1) to.roots.push(...ids);
      else to.roots.unshift(...ids);
      for (const id of ids) {
        s.byId[id].parent = null;
        reassignPage(s, id, toPage);
      }
    })
  );
}


/** Resolve the adjacent feed day for a root block at the page boundary, loading
 *  older days if a down-move runs off the last loaded one. Returns the target
 *  page name, or null if there's nowhere to go. */
async function feedNeighbor(page: string, dir: 1 | -1): Promise<string | null> {
  let fi = doc.feed.indexOf(page);
  if (fi < 0) return null; // not a feed day (e.g. a named page)
  let ti = fi + dir;
  if (ti < 0) return null; // top of the feed (today) — can't go higher
  if (ti >= doc.feed.length) {
    if (dir !== 1 || !feedExtender || !(await feedExtender())) return null;
    fi = doc.feed.indexOf(page);
    ti = fi + dir;
    if (ti < 0 || ti >= doc.feed.length) return null;
  }
  return doc.feed[ti];
}

/** Like `nextVisible`, but when we're at the last LOADED block of the journal feed
 *  it pulls in the next day first (via the feed extender) and returns that day's
 *  first block. This lets Down-arrow keep going past the loaded window — previously
 *  only mouse-wheel scrolling (the LoadMore sentinel) grew the feed, so keyboard nav
 *  dead-ended at the last loaded bullet. Resolves to null when there's genuinely
 *  nothing below (a non-feed page, or the feed is exhausted). */
export async function nextVisibleOrExtend(id: string): Promise<string | null> {
  const direct = nextVisible(id);
  if (direct) return direct;
  const node = doc.byId[id];
  if (!node || doc.feed.indexOf(node.page) < 0) return null; // not a feed day → nothing to load
  if (!feedExtender || !(await feedExtender())) return null; // feed exhausted / no extender
  return nextVisible(id); // the newly-appended day's first block is now loaded
}

/** Pull in the next journal-feed day if there is one; resolves to whether the feed
 *  actually grew. Used by scroll-restore to reach a saved offset that lives in
 *  not-yet-loaded days (the feed otherwise only grows on a mouse-wheel sentinel
 *  hit). No-op (false) on a non-feed page or when the feed is exhausted. */
export async function extendFeedForScroll(): Promise<boolean> {
  return feedExtender ? feedExtender() : false;
}

/** Move a single block one slot, crossing into the adjacent day at a page
 *  boundary. Returns how it moved so the caller can restore the caret. */
async function moveBlockFeedNow(id: string, dir: 1 | -1): Promise<"within" | "crossed" | "none"> {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return "none";
  if (canMoveItem(id, dir)) {
    moveItem(id, dir);
    return "within";
  }
  if (node.parent !== null) return "none"; // nested block at a child-list edge: stop
  const target = await feedNeighbor(node.page, dir);
  if (!target || !pageWritable(target)) return "none";
  const outcome = await requestCrossPageMove<{ page: string }>({
    plan: () => {
      // Re-read the LIVE node: it may have vanished, been nested, or already
      // crossed while the pre-flush awaited.
      const live = doc.byId[id];
      if (!live || !blockWritable(id) || live.parent !== null) return null;
      if (live.page === target) return null;
      return { page: live.page };
    },
    intent: (current) => ({ sourcePages: [current.page], destinationPage: target, roots: [id] }),
    apply: (current) => {
      pushUndo("move-cross", [current.page, target]);
      crossMoveBlocks([id], current.page, target, dir);
      return true;
    },
  });
  return outcome.applied ? "crossed" : "none";
}

export function moveBlockFeed(id: string, dir: 1 | -1): Promise<"within" | "crossed" | "none"> {
  return moveBlockFeedNow(id, dir);
}

/** Move every top-level selected block up/down by one slot, preserving the
 *  selection; at a day boundary the whole group crosses into the adjacent day. */
export async function moveSelectionItems(dir: 1 | -1) {
  const ids = topSelected(); // document order: ids[0] topmost, last bottommost
  if (!ids.length || ids.some((id) => !blockWritable(id))) return;
  const lead = dir === 1 ? ids[ids.length - 1] : ids[0];
  if (canMoveSelectionWithinSiblings(lead, dir)) {
    // Batch the whole selection into ONE undo entry + ONE produce. Doing it
    // per-block (a moveItem call each) snapshots the entire working set K times —
    // a 15-block nudge became 15 full clones, the visible jank. Going down, move
    // the bottom-most first so they don't collide; up, the top.
    const ordered = dir === 1 ? [...ids].reverse() : ids;
    const pages = beginOrContinueMoveSelectionUndo(ids);
    if (!pages) return;
    setDoc(
      produce((s) => {
        for (const id of ordered) {
          const node = s.byId[id];
          if (!node) continue;
          const arr =
            node.parent === null
              ? s.pages[s.pages.findIndex((p) => p.name === node.page)].roots
              : s.byId[node.parent].children;
          const i = arr.indexOf(id);
          const ni = i + dir;
          if (i < 0 || ni < 0 || ni >= arr.length) continue;
          arr.splice(i, 1);
          arr.splice(ni, 0, id);
        }
      })
    );
    for (const p of pages) markDirty(p);
    return;
  }
  // Boundary: cross the whole group into the adjacent day (only if every
  // selected block is a root block on the same feed day).
  // This route awaits source durability and has its own cross-page snapshot;
  // never let it borrow an in-page burst's inverse across that await.
  endMoveSelectionBurst();
  const page = doc.byId[ids[0]]?.page;
  if (!page) return;
  if (ids.some((id) => doc.byId[id].parent !== null || doc.byId[id].page !== page)) return;
  const target = await feedNeighbor(page, dir);
  if (!target || !pageWritable(target)) return;
  // The door re-runs this plan after the pre-flush await and compares it. That
  // is what stops a second key repeat from appending roots the target day
  // already holds (H3): by then the roots live on `target`, so the plan is null.
  await requestCrossPageMove<{ page: string; ids: string[] }>({
    plan: () => {
      const live = topSelected();
      if (!live.length || live.some((sel) => !blockWritable(sel))) return null;
      const from = doc.byId[live[0]]?.page;
      if (!from || from === target) return null;
      if (live.some((sel) => doc.byId[sel].parent !== null || doc.byId[sel].page !== from)) return null;
      return { page: from, ids: live };
    },
    intent: (current) => ({ sourcePages: [current.page], destinationPage: target, roots: current.ids }),
    apply: (current) => {
      pushUndo("move-sel-cross", [current.page, target]);
      crossMoveBlocks(current.ids, current.page, target, dir);
      return true;
    },
  });
}

// ---------------------------------------------------------------------------
// Carry unfinished tasks forward (B)
// ---------------------------------------------------------------------------

function isOpenTask(id: string): boolean {
  // Leading task marker via the one markers.ts recognizer — byte-equivalent to
  // what the lsdoc boundary projects (DUP-7), so carry and the rendered checkbox
  // cannot disagree — and parser-free, so carry works without the wasm renderer up.
  const m = leadingMarker(doc.byId[id]?.raw ?? "");
  return !!m && OPEN_MARKERS.has(m);
}
function subtreeHasOpenTask(id: string): boolean {
  const n = doc.byId[id];
  if (!n) return false;
  return isOpenTask(id) || n.children.some(subtreeHasOpenTask);
}
/** Collect the top-most open-task blocks in a subtree (open tasks not nested
 *  under another open task) — the pull-out unit when keepContext is off. */
function collectTopOpenTasks(id: string, acc: string[]) {
  if (isOpenTask(id)) {
    acc.push(id);
    return; // its open-task descendants travel with it
  }
  for (const c of doc.byId[id]?.children ?? []) collectTopOpenTasks(c, acc);
}

/** Carry unfinished tasks from `fromPages` into today's journal. Pages are
 *  processed in the given order and each batch is appended, so passing days
 *  newest→oldest puts the newest on top. `keepContext` true moves each top-level
 *  block that contains an open task whole; false pulls out just the open-task
 *  subtrees. Returns the number of blocks moved. Today + every fromPage must be
 *  loaded into the working set first. */
export function carryUnfinished(
  fromPages: string[],
  keepContext: boolean,
  header: string | null
): number {
  const today = journalTitle(new Date());
  if (!pageWritable(today) || fromPages.some((page) => pageByName(page) && !pageWritable(page))) return 0;
  type Item = { id: string; from: string; parent: string | null };
  const plan: Item[] = [];
  for (const fp of fromPages) {
    if (fp === today) continue;
    const page = pageByName(fp);
    if (!page) continue;
    if (keepContext) {
      for (const rid of page.roots) {
        if (subtreeHasOpenTask(rid)) plan.push({ id: rid, from: fp, parent: null });
      }
    } else {
      const ids: string[] = [];
      for (const rid of page.roots) collectTopOpenTasks(rid, ids);
      for (const id of ids) plan.push({ id, from: fp, parent: doc.byId[id].parent });
    }
  }
  if (!plan.length) return 0;
  pushUndo("carry", [today, ...new Set(plan.map((i) => i.from))]);
  setDoc(
    produce((s) => {
      const todayPage = s.pages.find((p) => p.name === today);
      if (!todayPage) return;
      const carried: string[] = [];
      for (const item of plan) {
        if (item.parent === null) {
          const pg = s.pages.find((p) => p.name === item.from);
          if (pg) pg.roots = pg.roots.filter((x) => x !== item.id);
        } else {
          const par = s.byId[item.parent];
          if (par) par.children = par.children.filter((x) => x !== item.id);
        }
        s.byId[item.id].parent = null;
        reassignPage(s, item.id, today);
        carried.push(item.id);
      }
      // NB: only the carried task blocks are removed from the source day. Anything
      // the user left behind — finished tasks, notes, and blank spacer bullets — is
      // never touched. (A blank bullet that only *held* a carried task is likewise
      // left in place; it never had a task marker itself.)
      // Drop today's lone empty placeholder bullet so carried tasks don't sit
      // under a blank line.
      if (todayPage.roots.length === 1) {
        const only = s.byId[todayPage.roots[0]];
        if (only && only.children.length === 0 && only.raw.trim() === "") {
          delete s.byId[todayPage.roots[0]];
          todayPage.roots = [];
        }
      }
      if (header) {
        const hid = freshId();
        s.byId[hid] = { id: hid, raw: header, collapsed: false, parent: null, page: today, children: [] };
        todayPage.roots.push(hid);
      }
      todayPage.roots.push(...carried);
    })
  );
  // Persistence — the barrier, the destination-first order and the record — is
  // the front door's, exactly as it is for every other cross-page shape. This
  // function is pure tree surgery and reports how much of it happened.
  return plan.length;
}
