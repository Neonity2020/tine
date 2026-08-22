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
  type JSX,
} from "solid-js";
import { backend } from "../backend";
import {
  clearConflict,
  conflictQueue,
  pushToast,
  refreshSyncConflicts,
  updateLiveSaveConflictDiskRev,
} from "../ui";
import { dropObservation, reobserve } from "../persistence";
import { reloadPage } from "../store";
import { DiffRowView, collectRows, seedSuggestedOrNoLoss } from "./DiffRows";
import type { ConflictObject, DiffRow, MergeDecision, SyncConflictDiff } from "../types";

/** The side labels the artifact itself supplied, shortened for a row segment. */
function segLabel(text: string, fallback: string): string {
  const trimmed = text.trim();
  if (!trimmed) return fallback;
  return trimmed.length > 18 ? `${trimmed.slice(0, 17)}…` : trimmed;
}

/** What the two sides of this conflict are called, from the queue object. */
function sideLabels(conflict: ConflictObject): { mine: string; theirs: string; base?: string } {
  const of = (role: "mine" | "theirs" | "base") =>
    conflict.sides.find((s) => s.role === role)?.label ?? "";
  return {
    mine: of("mine") || (conflict.source === "vcs-markers" ? "Local side" : "This device"),
    theirs: of("theirs") || (conflict.source === "vcs-markers" ? "Merged-in side" : "Conflict copy"),
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
    if (current) setDecisions(seedSuggestedOrNoLoss(current.rows));
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
    const pageName = c.page_name;
    const pagePath = c.page_path;
    const sides = [...c.sides];
    const live = c.live;
    setBusy(true);
    try {
      if (source === "live-save") {
        if (!live) return;
        const resolved = live.disk_rev !== undefined
          ? await backend().resolveDurableLiveSaveConflict(
            live.page,
            live.disk_rev,
            decisions(),
            preChoice(),
          )
          : await backend().resolveLiveSaveConflict(
            live.page,
            live.base_rev,
            live.conflict_epoch,
            decisions(),
            preChoice(),
          );
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
        await backend().resolveSyncConflict(
          pagePath,
          copy,
          decisions(),
          current.base_rev,
          current.conflict_rev,
          preChoice()
        );
        pushToast(`Merged into “${pageName}”`, "success");
      }
      await refreshSyncConflicts();
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
    <div class="page-conflict" ref={root}>
      <div class="page-conflict-head">
        <span class="page-conflict-title">
          {conflict().source === "vcs-markers"
            ? "Unresolved merge from your version-control tool"
            : conflict().source === "live-save"
              ? "Your draft and the current file both changed"
            : "Two versions of this page arrived"}
        </span>
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
        <span class="page-conflict-side theirs">{labels().theirs}</span>
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
                <button class="settings-btn" onClick={applyAllSuggested}>
                  Apply all suggested
                </button>
                <button class="settings-btn" onClick={() => setAll("both")}>
                  Keep both everywhere
                </button>
                <button class="settings-btn" onClick={() => setAll("mine")}>
                  Keep {segLabel(labels().mine, "mine")}
                </button>
                <button class="settings-btn" onClick={() => setAll("theirs")}>
                  Keep {segLabel(labels().theirs, "theirs")}
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
  );
}
