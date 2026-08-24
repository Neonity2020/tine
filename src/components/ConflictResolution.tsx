// Concord L4 — resolve a conflict AT the page, not in a Settings modal.
//
// The queue (`conflictQueue`) says which pages need judgement; this renders the
// one for the page you are looking at. Both artifact sources land here: a sync
// transport's conflict copy, and a VCS merge's `<<<<<<<` markers parsed out of
// the file itself (L5). They differ only in where the two sides come from and
// which guarded backend path applies them — the review is identical, and the
// rows are the shared `DiffRowView`, never a second renderer.
//
// Nothing here ever auto-applies. The base (Concord ledger, or the markers' own
// `|||||||` ancestor) only decides which side arrives PRE-SELECTED; the write
// happens on the user's click, through `resolve_sync_conflict` /
// `resolve_vcs_marker_conflict` with their page lock, `base_rev` guard and org
// firewall intact.
import {
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
  onMount,
  type JSX,
} from "solid-js";
import { backend } from "../backend";
import {
  clearConflict,
  conflictQueue,
  pushToast,
  refreshLiveSaveConflictDraft,
  refreshSyncConflicts,
  settleArtifactConflict,
  updateLiveSaveConflictDiskRev,
} from "../ui";
import {
  dropObservation,
  flushPageToQuiescence,
  holdManagedMovePages,
  isDirty,
  isSaving,
  reobserve,
} from "../persistence";
import {
  editGeneration,
  editorTransactionGeneration,
  hasEditorLease,
  holdPageMutationUi,
  pageToDto,
  pageInstanceGeneration,
  reloadPage,
} from "../store";
import {
  DiffRowView,
  collectRows,
  humanizeSideLabel,
  seedSuggestedExceptArtifact,
  seedSuggestedOrNoLoss,
} from "./DiffRows";
import type { ConflictObject, DiffRow, MergeDecision, PageDto, SyncConflictDiff } from "../types";

/** The side labels the artifact itself supplied, shortened for a row segment. */
function segLabel(text: string, fallback: string): string {
  const trimmed = text.trim();
  if (!trimmed) return fallback;
  return trimmed.length > 18 ? `${trimmed.slice(0, 17)}…` : trimmed;
}

/** What the two sides of this conflict are called, from the queue object.
 *  A sync tool's raw copy tag is humanized for display ("Sync copy · Jul 5");
 *  the exact tag survives as `theirsTitle` for the legend's tooltip. */
function sideLabels(conflict: ConflictObject): {
  mine: string;
  theirs: string;
  theirsTitle?: string;
  base?: string;
} {
  const of = (role: "mine" | "theirs" | "base") =>
    conflict.sides.find((s) => s.role === role)?.label ?? "";
  const theirs = humanizeSideLabel(
    of("theirs") || (conflict.source === "vcs-markers" ? "Merged-in side" : "Conflict copy")
  );
  return {
    mine: of("mine") || (conflict.source === "vcs-markers" ? "Local side" : "This device"),
    theirs: theirs.text,
    theirsTitle: theirs.title,
    base: of("base") || undefined,
  };
}

/** The in-page conflict resolver for the page currently being viewed. */
export function PageConflictResolution(props: { conflict: ConflictObject }): JSX.Element {
  const conflict = () => props.conflict;
  // These survive removal of the surrounding `<Show>`. Cleanup runs precisely
  // while that owner is being disposed, when reading `props.conflict` again is
  // a stale reactive access.
  const cleanupConflictId = props.conflict.id;
  const cleanupPageName = props.conflict.page_name;
  let mounted = true;
  const labels = createMemo(() => sideLabels(conflict()));
  const [decisions, setDecisions] = createSignal<Record<string, MergeDecision>>({});
  // The page-header (pre-block) properties are one decision for the whole page,
  // not a row — `alias::`/`tags::` lines aren't outline blocks. Keeping BOTH is
  // the no-loss default, consistent with this surface's row policy; the other
  // two options exist because a union is not always what the user wants.
  const [preChoice, setPreChoice] = createSignal<"mine" | "theirs" | "union">("union");
  const [showUnchanged, setShowUnchanged] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [cursor, setCursor] = createSignal(0);
  let root: HTMLDivElement | undefined;

  // --- Dock: slim pinned bar + unroll-in-place sheet ---------------------
  // (Design + argued decisions: tine-agents/specs/concord-conflict-dock.md.)
  // The panel lives at the top of the page and scrolls away with it; on a
  // phone that made a conflict invisible until the user happened to scroll
  // up. When the panel is ENTIRELY above the viewport, a slim bar pins to
  // the top of this pane's scroller; tapping it unrolls the SAME panel node
  // (physically reparented, so decisions/diff/DOM state survive) as a pinned
  // sheet. Fixed positioning, not sticky: WebKitGTK has no scroll anchoring,
  // so an in-flow height swap would visibly jump the content.
  const [docked, setDocked] = createSignal(false);
  const [expanded, setExpanded] = createSignal(false);
  const [dockRect, setDockRect] = createSignal<{
    left: number;
    top: number;
    width: number;
  } | null>(null);
  let inlineSlot: HTMLDivElement | undefined;
  let sentinel: HTMLDivElement | undefined;
  let sheetEl: HTMLDivElement | undefined;

  const paneScroller = () => inlineSlot?.closest(".main-content") ?? null;
  const measureDock = () => {
    const scroller = paneScroller();
    if (!scroller) {
      setDockRect(null);
      return;
    }
    const r = scroller.getBoundingClientRect();
    setDockRect({ left: r.left, top: r.top, width: r.width });
  };

  onMount(() => {
    // The sentinel sits directly BELOW the inline panel: dock only when it is
    // entirely above the viewport (a tall, half-visible panel must not dock;
    // neither must a sentinel still below the fold on a short window).
    if (!sentinel || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver((entries) => {
      const entry = entries[entries.length - 1];
      if (!entry) return;
      const above = !entry.isIntersecting && entry.boundingClientRect.top < 0;
      setDocked(above);
      if (!above) setExpanded(false); // panel back in view: the bar yields
    });
    io.observe(sentinel);
    onCleanup(() => io.disconnect());
  });

  // Keep the fixed dock aligned with THIS pane's scroller (split panes each
  // dock over their own content column).
  createEffect(() => {
    if (!docked()) return;
    measureDock();
    const onResize = () => measureDock();
    window.addEventListener("resize", onResize);
    let ro: ResizeObserver | undefined;
    const scroller = paneScroller();
    if (typeof ResizeObserver !== "undefined" && scroller) {
      ro = new ResizeObserver(onResize);
      ro.observe(scroller);
    }
    onCleanup(() => {
      window.removeEventListener("resize", onResize);
      ro?.disconnect();
    });
  });

  // ONE panel node, moved — never a second render of the rows. While the
  // panel is in the sheet, its inline slot keeps the vacated height so the
  // content below does not jump (same no-anchoring reason as above).
  createEffect(() => {
    const panel = root;
    if (!panel || !inlineSlot) return;
    if (docked() && expanded() && sheetEl) {
      inlineSlot.style.minHeight = `${panel.offsetHeight}px`;
      sheetEl.appendChild(panel);
    } else if (panel.parentElement !== inlineSlot) {
      inlineSlot.appendChild(panel);
      inlineSlot.style.minHeight = "";
    }
  });

  const conflictTitle = () =>
    conflict().source === "vcs-markers"
      ? "Unresolved merge from your version-control tool"
      : conflict().source === "live-save"
        ? "Your draft and the current file both changed"
        : "Two versions of this page arrived";

  // Load the diff from whichever source this object came from. Both return the
  // same `SyncConflictDiff`, so everything downstream is source-agnostic.
  const [diff, { refetch }] = createResource<SyncConflictDiff | null, string>(
    () => `${conflict().id}:${conflict().live?.draft_version ?? 0}`,
    async () => {
      const c = conflict();
      if (c.source === "live-save") {
        const live = c.live;
        if (!live) return null;
        const result = live.disk_rev !== undefined
          ? await backend().durableLiveSaveConflictDiff(live.page, live.base_text ?? null)
          : await backend().liveSaveConflictDiff(
              live.page,
              live.base_rev,
              live.conflict_epoch,
            );
        updateLiveSaveConflictDiskRev(c.page_name, result.conflict_rev);
        return result;
      }
      if (c.source === "vcs-markers") {
        const parsed = await backend().vcsMarkerConflictDiff(c.page_path);
        return parsed?.diff ?? null;
      }
      const copy = c.sides.find((s) => s.role === "theirs")?.path;
      if (!copy) return null;
      return await backend().syncConflictDiff(c.page_path, copy);
    }
  );

  // Every fresh alignment restarts from the suggested resolution (and the
  // no-loss choice where there is no suggestion). Row decisions belong to ONE
  // exact pair of texts, so they are never carried across a refetch.
  let alignment: string | undefined;
  createEffect(() => {
    const current = diff();
    if (!current) return;
    const next = `${current.base_rev}\0${current.conflict_rev}`;
    if (alignment !== next) {
      setDecisions(seedSuggestedOrNoLoss(current.rows));
      setPreChoice("union");
      setCursor(0);
    }
    alignment = next;
  });

  const pending = createMemo(() => collectRows(diff()?.rows ?? []));
  const countSuggestions = (rows: DiffRow[]): number =>
    rows.reduce((n, r) => n + (r.suggestion ? 1 : 0) + countSuggestions(r.children), 0);
  const suggestedCount = createMemo(() => countSuggestions(diff()?.rows ?? []));

  const setDecision = (id: string, d: MergeDecision) => setDecisions((m) => ({ ...m, [id]: d }));
  const setAll = (d: MergeDecision) => {
    const next: Record<string, MergeDecision> = {};
    for (const { id } of pending()) next[id] = d;
    setDecisions(next);
  };
  const applyAllSuggested = () => {
    const current = diff();
    if (!current) return;
    // The sweep re-applies Tine's OWN suggestions. A merge tool's proposed
    // text (an artifact-source merged row) keeps whatever the user set —
    // Tine vouches for nothing about that text, so accepting it stays a
    // per-row act (the initial pre-selection still stands until touched).
    setDecisions((prev) => seedSuggestedExceptArtifact(current.rows, { ...prev }));
  };

  /** Move the highlight to the previous/next region that needs a decision. */
  const step = (delta: number) => {
    const rows = pending();
    if (!rows.length) return;
    const at = (cursor() + delta + rows.length) % rows.length;
    setCursor(at);
    const el = root?.querySelector(`[data-row-id="${CSS.escape(rows[at].id)}"]`);
    el?.scrollIntoView({ block: "center" });
    (el as HTMLElement | undefined)?.classList.add("page-conflict-row-focus");
    window.setTimeout(() => el?.classList.remove("page-conflict-row-focus"), 900);
  };

  const apply = async () => {
    const current = diff();
    if (!current || diff.loading || busy()) return;
    const c = conflict();
    // `c` belongs to the surrounding Solid <Show>. Resolving or refreshing can
    // remove that owner immediately, so keep only plain snapshots across IPC
    // and reactive mutations. Reading `c` afterwards is a stale-owner access.
    const source = c.source;
    const conflictId = c.id;
    const pageName = c.page_name;
    const pagePath = c.page_path;
    const sides = [...c.sides];
    const live = c.live;
    setBusy(true);
    let releasePageUi = () => {};
    let releasePageSaves = () => {};
    try {
      if (source === "live-save") {
        if (!live) return;
        // The diff is an exact review of `live.page`, not a standing grant to
        // write whatever happened to be in the editor when Apply was clicked.
        // Freeze this page, then prove the current draft is still the reviewed
        // one before crossing the native write boundary. This closes the normal
        // debounce window where post-conflict typing had not yet refreshed the
        // stored capsule and was silently replaced by the resolved DTO.
        if (hasEditorLease(pageName)) {
          pushToast("Finish the current edit, then apply this resolution.", "info");
          return;
        }
        releasePageUi = holdPageMutationUi([pageName]);
        const instance = pageInstanceGeneration(pageName);
        const edit = editGeneration(pageName);
        const transaction = editorTransactionGeneration(pageName);
        const reviewedDraft = pageToDto(pageName);
        if (!reviewedDraft || instance === null) {
          pushToast("This page is no longer open. Open it again before applying the resolution.", "error");
          return;
        }
        if (
          pageInstanceGeneration(pageName) !== instance
          || editGeneration(pageName) !== edit
          || editorTransactionGeneration(pageName) !== transaction
          || hasEditorLease(pageName)
        ) {
          alignment = undefined;
          void refetch();
          pushToast("The open page changed while Tine prepared the merge. Review the refreshed comparison.", "info");
          return;
        }
        // Both snapshots came from pageToDto. Ignore only backend-populated
        // revision metadata; every editable semantic/identity field must match.
        const snapshot = (page: PageDto) => JSON.stringify({
          name: page.name,
          kind: page.kind,
          title: page.title,
          pre_block: page.pre_block,
          blocks: page.blocks,
          format: page.format ?? "md",
          path: page.path ?? "",
          guide: page.guide ?? false,
          read_only: page.read_only ?? false,
        });
        // After restart, the editor is deliberately loaded from the current
        // disk winner while `live.page` is the only retained copy of the user's
        // unsaved draft. The disk-loaded editor must never overwrite that
        // capsule merely because their contents differ. If the user has since
        // edited the reopened disk page, preserve both and require a deliberate
        // follow-up policy; otherwise Apply resolves the exact retained draft
        // whose comparison is on screen.
        if (live.restored && isDirty(pageName)) {
          pushToast(
            "This reopened page also has new edits. Finish or preserve them before resolving the recovered draft.",
            "info",
          );
          return;
        }
        if (!live.restored && snapshot(reviewedDraft) !== snapshot(live.page)) {
          refreshLiveSaveConflictDraft(reviewedDraft);
          alignment = undefined;
          void refetch();
          pushToast("Your draft changed. Review the updated comparison, then apply it again.", "info");
          return;
        }
        const draftToResolve = live.restored ? live.page : reviewedDraft;
        releasePageSaves = holdManagedMovePages([pageName]);
        const resolved = live.disk_rev !== undefined
          ? await backend().resolveDurableLiveSaveConflict(
            draftToResolve,
            live.disk_rev,
            decisions(),
            preChoice(),
          )
          : await backend().resolveLiveSaveConflict(
            draftToResolve,
            live.base_rev,
            live.conflict_epoch,
            decisions(),
            preChoice(),
          );
        if (
          pageInstanceGeneration(pageName) !== instance
          || editGeneration(pageName) !== edit
          || editorTransactionGeneration(pageName) !== transaction
        ) {
          throw new Error("the resolved file was committed, but the open editor changed; reopen the page to load the exact result");
        }
        // The guarded native command is the durable resolution boundary. Clear
        // the capsule there, before editor replacement waits to retire the old
        // activation: retirement can be slow, but it must not keep presenting a
        // conflict whose merged bytes are already committed. The returned DTO
        // is the exact page written, so installing it cannot invent another
        // version even if that retirement finishes later.
        dropObservation(pageName);
        clearConflict(pageName);
        await reloadPage(resolved);
        pushToast(`Resolved the live conflict in “${pageName}”`, "success");
      } else if (source === "vcs-markers") {
        await backend().resolveVcsMarkerConflict(
          pagePath,
          decisions(),
          current.base_rev,
          preChoice()
        );
        pushToast(`Resolved the merge in “${pageName}”`, "success");
      } else {
        const copy = sides.find((s) => s.role === "theirs")?.path;
        if (!copy) return;
        // The resolver writes the winning file. It must therefore own the exact
        // open editor from the user's click until the committed DTO is installed;
        // otherwise the pre-merge editor can autosave over that merge and create
        // the confusing second conflict this surface is meant to eliminate.
        if (hasEditorLease(pageName)) {
          pushToast("Finish the current edit, then apply this resolution.", "info");
          return;
        }
        const instance = pageInstanceGeneration(pageName);
        const edit = editGeneration(pageName);
        const transaction = editorTransactionGeneration(pageName);
        const hadPendingSave = isDirty(pageName) || isSaving(pageName);
        if (instance === null) {
          pushToast("This page is no longer open. Open it again before applying the resolution.", "error");
          return;
        }
        releasePageUi = holdPageMutationUi([pageName]);
        if (!(await flushPageToQuiescence(pageName))) {
          pushToast("Your current draft needs attention first. Resolve or finish saving it, then review this conflict copy.", "error");
          return;
        }
        if (
          pageInstanceGeneration(pageName) !== instance
          || editGeneration(pageName) !== edit
          || editorTransactionGeneration(pageName) !== transaction
          || hasEditorLease(pageName)
        ) {
          alignment = undefined;
          void refetch();
          pushToast("The open page changed while Tine prepared the merge. Review the refreshed comparison.", "info");
          return;
        }
        if (hadPendingSave) {
          alignment = undefined;
          await refetch();
          pushToast("Your latest edit was saved. Review the updated comparison, then apply it again.", "info");
          return;
        }
        releasePageSaves = holdManagedMovePages([pageName]);
        const resolved = await backend().resolveSyncConflict(
          pagePath,
          copy,
          decisions(),
          current.base_rev,
          current.conflict_rev,
          current.merge_base_rev ?? null,
          preChoice()
        );
        if (
          pageInstanceGeneration(pageName) !== instance
          || editGeneration(pageName) !== edit
          || editorTransactionGeneration(pageName) !== transaction
        ) {
          throw new Error("the resolved file was committed, but the open editor changed; reopen the page to load the exact result");
        }
        dropObservation(pageName);
        clearConflict(pageName);
        const refusal = await reloadPage(resolved);
        if (refusal) {
          throw new Error("the resolved file was committed, but its exact page could not replace the old editor; reopen the page");
        }
        settleArtifactConflict(conflictId);
        pushToast(`Merged into “${pageName}”`, "success");
      }
      // A successful sync-copy commit has already retired its exact object
      // locally. Reconcile unrelated artifacts without keeping Apply blocked on
      // graph-wide directory walks (noticeable on Android document trees).
      if (source === "sync-copy") void refreshSyncConflicts();
      else await refreshSyncConflicts();
    } catch (e) {
      if (String(e).includes("conflict")) {
        pushToast("The file changed on disk — re-reading it, please redo your choices.", "error");
        alignment = undefined;
        if (source === "live-save") {
          dropObservation(pageName);
          await reobserve(pageName);
        } else {
          void refetch();
        }
      } else {
        pushToast(`Couldn’t resolve it: ${String(e)}`, "error");
      }
    } finally {
      releasePageSaves();
      releasePageUi();
      if (mounted) setBusy(false);
    }
  };

  // Leaving the page with work outstanding gets a quiet note, never a dialog
  // that blocks navigation (L3: a conflict is a calm object, not a modal).
  onCleanup(() => {
    mounted = false;
    if (conflictQueue().some((q) => q.id === cleanupConflictId)) {
      pushToast(`“${cleanupPageName}” still has unresolved conflicts`, "info");
    }
  });

  return (
    <>
    <div class="page-conflict-slot" ref={inlineSlot}>
    <div class="page-conflict" ref={root}>
      <div class="page-conflict-head">
        <span class="page-conflict-title">{conflictTitle()}</span>
        <span class="page-conflict-nav">
          <Show when={pending().length}>
            <span class="page-conflict-count">
              {pending().length} conflict{pending().length === 1 ? "" : "s"}
            </span>
            <button class="settings-btn" title="Previous conflict" onClick={() => step(-1)}>
              ↑
            </button>
            <button class="settings-btn" title="Next conflict" onClick={() => step(1)}>
              ↓
            </button>
          </Show>
        </span>
      </div>
      <div class="page-conflict-legend">
        <span class="page-conflict-side mine">{labels().mine}</span>
        <span class="page-conflict-side theirs" title={labels().theirsTitle}>
          {labels().theirs}
        </span>
        <Show when={labels().base}>
          {(base) => <span class="page-conflict-side base">{base()} (used for the suggestions)</span>}
        </Show>
      </div>
      <Show
        when={diff()}
        fallback={
          <div class="page-conflict-empty">
            {diff.loading ? "Reading both versions…" : "Couldn’t read this conflict."}
          </div>
        }
      >
        {(d) => (
          <Show
            when={!d().blocks_identical || d().pre_differs}
            fallback={
              <div class="page-conflict-empty">
                The two versions are identical — nothing to decide.
                <Show when={conflict().source === "sync-copy"}>
                  {" "}The copy is safe to discard in Settings → Backups &amp; recovery.
                </Show>
              </div>
            }
          >
            <div class="sync-merge-toolbar">
              <span class="settings-hint">
                <Show
                  when={suggestedCount()}
                  fallback={
                    <>Nothing was pre-selected — no common version is known, so both sides are kept.</>
                  }
                >
                  {suggestedCount()} of {pending().length} pre-selected from the last version both
                  sides agreed on — review and confirm.
                </Show>
              </span>
              <span class="sync-merge-toolbar-actions">
                <button
                  class="settings-btn"
                  onClick={applyAllSuggested}
                  title="Re-applies Tine's own suggestions. A merge tool's proposed text (Merged (tool)) keeps your current choice."
                >
                  <span class="conflict-wide">Apply all suggested</span>
                  <span class="conflict-narrow">All suggested</span>
                </button>
                <button class="settings-btn" onClick={() => setAll("both")}>
                  <span class="conflict-wide">Keep both everywhere</span>
                  <span class="conflict-narrow">All both</span>
                </button>
                <button
                  class="settings-btn"
                  onClick={() => setAll("mine")}
                  title={labels().mine}
                >
                  <span class="conflict-wide">Keep {segLabel(labels().mine, "mine")}</span>
                  <span class="conflict-narrow">
                    <span class="sync-merge-seg-dot" data-side="mine" aria-hidden="true" />
                    All mine
                  </span>
                </button>
                <button
                  class="settings-btn"
                  onClick={() => setAll("theirs")}
                  title={labels().theirsTitle ?? labels().theirs}
                >
                  <span class="conflict-wide">Keep {segLabel(labels().theirs, "theirs")}</span>
                  <span class="conflict-narrow">
                    <span class="sync-merge-seg-dot" data-side="theirs" aria-hidden="true" />
                    All theirs
                  </span>
                </button>
                <label class="sync-merge-showunchanged">
                  <input
                    type="checkbox"
                    checked={showUnchanged()}
                    onChange={(e) => setShowUnchanged(e.currentTarget.checked)}
                  />
                  show unchanged
                </label>
              </span>
            </div>
            <div class="sync-merge-collabels">
              <span>{labels().mine}</span>
              <span>{labels().theirs}</span>
            </div>
            <div class="page-conflict-rows">
              <For each={d().rows}>
                {(row) => (
                  <DiffRowView
                    row={row}
                    depth={0}
                    decisions={decisions()}
                    setDecision={setDecision}
                    showUnchanged={showUnchanged()}
                    fallback="both"
                    labels={{
                      mine: segLabel(labels().mine, "Mine"),
                      theirs: segLabel(labels().theirs, "Theirs"),
                    }}
                  />
                )}
              </For>
            </div>
            <Show when={d().pre_differs}>
              <div class="sync-merge-preblock">
                <div class="settings-hint">
                  The page’s own properties differ. Keep{" "}
                  <select
                    class="page-conflict-preblock-choice"
                    value={preChoice()}
                    onChange={(e) =>
                      setPreChoice(e.currentTarget.value as "mine" | "theirs" | "union")
                    }
                  >
                    <option value="union">both (merge)</option>
                    <option value="mine">{segLabel(labels().mine, "mine")}</option>
                    <option value="theirs">{segLabel(labels().theirs, "theirs")}</option>
                  </select>
                </div>
              </div>
            </Show>
            <div class="page-conflict-foot">
              <span class="settings-hint">
                <Show
                  when={conflict().source === "vcs-markers"}
                  fallback={
                    conflict().source === "live-save"
                      ? <>The resolved page is saved through the same guarded Direct Files path.</>
                      : <>The copy moves to the recoverable trash once this is applied.</>
                  }
                >
                  Applying writes the merged page without any markers — the file becomes ordinary
                  Markdown again and saves are no longer refused.
                </Show>
              </span>
              <button
                class="settings-btn settings-btn-primary"
                disabled={busy() || diff.loading}
                onClick={() => void apply()}
              >
                {busy() ? "Applying…" : "Apply resolution"}
              </button>
            </div>
          </Show>
        )}
      </Show>
    </div>
    </div>
    <div class="page-conflict-sentinel" ref={sentinel} aria-hidden="true" />
    <Show when={docked()}>
      <div
        class="page-conflict-dock"
        classList={{ expanded: expanded() }}
        style={
          dockRect()
            ? {
                left: `${dockRect()!.left}px`,
                top: `${dockRect()!.top}px`,
                width: `${dockRect()!.width}px`,
              }
            : undefined
        }
        onKeyDown={(e) => {
          if (e.key === "Escape" && expanded()) {
            e.stopPropagation();
            setExpanded(false);
          }
        }}
      >
        <button
          class="page-conflict-dockbar"
          aria-expanded={expanded()}
          onClick={() => setExpanded(!expanded())}
        >
          <span class="page-conflict-dockbar-icon" aria-hidden="true">⚠</span>
          <span class="page-conflict-dockbar-title">{conflictTitle()}</span>
          <Show when={pending().length}>
            <span class="page-conflict-dockbar-count">
              {pending().length} to review
            </span>
          </Show>
          <span class="page-conflict-dockbar-chevron" aria-hidden="true">
            {expanded() ? "▴" : "▾"}
          </span>
        </button>
        <Show when={expanded()}>
          <div class="page-conflict-sheet" ref={(el) => (sheetEl = el)} />
        </Show>
      </div>
    </Show>
    </>
  );
}
