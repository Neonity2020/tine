import type { ActivationIntent, BlockDto, EditorActivationHandle, Format, PageDto, PageKind } from "../types";
import type { Route } from "../router";
import { FeedPage, Node, activatePageInstance, doc, mayReplaceInstance, pageByName, pageInstanceGeneration, peekPageInstanceGeneration, purgePageNodes, retirePageInstance, setDoc } from "./doc";
import { backend } from "../backend";
import { conflicts, rightSidebar } from "../ui";
import { dirtyPages, graphBinding, savingPages, setBaseRev, untombstone } from "../persistence";
import { editingId } from "../editorController";
import { facetsFromDto, seedFacets } from "../render/facets";
import { invalidateAllMatrixDimensions } from "../sheet/matrix";
import { invalidateUndoForPage } from "./undo";
import { onGraphRebound } from "../modeHooks";
import { produce } from "solid-js/store";


// ---------------------------------------------------------------------------
// Loading / serializing
// ---------------------------------------------------------------------------

function flatten(
  dtos: BlockDto[],
  parent: string | null,
  pageName: string,
  byId: Record<string, Node>,
  format: Format
): string[] {
  return dtos.map((d) => {
    // Seed the header-facet cache from the backend (one Rust lsdoc parse, shipped) so
    // the rendered chip reads off the DTO — zero frontend parse on load (M1 / P1).
    seedFacets(d.raw, format, facetsFromDto(d));
    // Cross-page id:: collision guard: if another LOADED page already owns this
    // id (two files share a persisted `id::` — copy-pasted raw, or a sync hiccup),
    // give this block a fresh store key instead of overwriting the other page's
    // node. Without this, the global byId entry is clobbered and saving one page
    // serializes the other's content. The block's raw (incl. its id:: line) is
    // untouched, so the file on disk is unchanged. Rust dedups ids WITHIN a page,
    // so this only fires across pages.
    const existing = byId[d.id];
    const key = existing && existing.page !== pageName ? `dup~${crypto.randomUUID()}` : d.id;
    const childIds = flatten(d.children, key, pageName, byId, format);
    byId[key] = {
      id: key,
      raw: d.raw,
      collapsed: d.collapsed,
      parent,
      page: pageName,
      children: childIds,
    };
    return key;
  });
}

/**
 * Live editor activations, keyed by page name.
 *
 * Deliberately NOT a field of `FeedPage`. `clonePages` spread-copies every field
 * of a page into history snapshots, so a token living on the page object would be
 * carried into every snapshot and reinstalled by `applyEntry` — handing a restored
 * editor a RETIRED activation, whose conflicts could then never be answered. An
 * activation identifies a live editor instance, and a copy of a page is not one.
 * (GH #254 increment 3; the failure was reproduced against a `FeedPage` field.)
 */
const editorActivations = new Map<string, number>();

/** The activation for `pageName`, if this page currently has a live editor. */
export function editorActivationFor(pageName: string): number | undefined {
  return editorActivations.get(pageName);
}

/** Record a freshly minted activation for `pageName`. */
export function setEditorActivation(pageName: string, activation: number): void {
  editorActivations.set(pageName, activation);
}

/**
 * Forget `pageName`'s activation locally, but only if it is still the one named.
 *
 * The local half of compare-and-retire: a retirement racing a newer activation
 * must not drop the newer one. The core is told separately, and its own
 * compare-and-retire is the authority.
 */
export function clearEditorActivation(pageName: string, activation?: number): boolean {
  const live = editorActivations.get(pageName);
  if (live === undefined) return false;
  if (activation !== undefined && live !== activation) return false;
  editorActivations.delete(pageName);
  prospectiveTargets.delete(pageName);
  return true;
}

/**
 * Prospective targets for editors activated with no file yet.
 *
 * Kept beside the activation registry rather than written onto the page: writing
 * it into the store mid-save was tried and reverted, because mutating the page
 * while a save is building its snapshot disturbs cut retirement, which is
 * authority-bound to the exact loaded instance. This is read at the DTO boundary
 * instead, which is where the core actually needs it — its drift/re-resolve
 * branch only runs for a pinned path. (GH #254 increment 3.)
 */
const prospectiveTargets = new Map<string, string>();

export function setProspectiveTarget(pageName: string, target: string): void {
  prospectiveTargets.set(pageName, target);
}

function recordEditorActivation(pageName: string, handle: EditorActivationHandle): void {
  setEditorActivation(pageName, handle.activation);
  // Keep the exact target beside a pathless editor after first creation too.
  // The save response is the first authoritative place the frontend learns the
  // resolved path, and `pageToDto` must keep pinning later saves to it.
  prospectiveTargets.set(pageName, handle.target);
}

async function retireExactEditorActivation(
  pageName: string,
  target: string | undefined,
  activation: number,
): Promise<void> {
  clearEditorActivation(pageName, activation);
  if (!target) return;
  await backend().retireEditorActivation(target, activation).catch(() => {});
}

async function retireMintedActivation(handle: EditorActivationHandle | null): Promise<void> {
  if (!handle) return;
  await backend().retireEditorActivation(handle.target, handle.activation).catch(() => {});
}

/** Drop every activation — graph reset and teardown. */
export function clearAllEditorActivations(): void {
  editorActivations.clear();
  prospectiveTargets.clear();
}

// A backend reopen installs a FRESH `Graph` whose activation registry is empty,
// so every token this side still holds names an editor the core has never heard
// of. Keeping them produced conflicts nobody could resolve: the ordinary save
// minted a banner carrying a retained token, and the matching force was refused
// `conflict_authority.superseded`, so BOTH banner buttons only re-observed into
// the same dead conflict. The registry has to be dropped with the graph that
// issued it. (GH #254 increment 3, round 15.)
onGraphRebound(clearAllEditorActivations);

/**
 * Retire `pageName`'s editor identity locally AND in the core.
 *
 * An activation that outlives its editor is not inert: with same-path activation
 * idempotence, a stale live token would be handed to the NEXT editor of that path,
 * which is exactly the cross-instance authority this increment exists to prevent.
 * So retirement is driven by the same events that retire the frontend instance —
 * eviction, `forgetPage`, reset — and is compare-and-retire on both sides, never a
 * bare "retire this path": a retirement racing a newer activation must not revoke
 * the newer editor. (GH #254 increment 3.)
 */
export function retireEditorFor(pageName: string, path?: string): void {
  const activation = editorActivations.get(pageName);
  if (activation === undefined) return;
  const target = path
    ?? doc.pages.find((p) => p.name === pageName)?.path
    ?? prospectiveTargets.get(pageName);
  void retireExactEditorActivation(pageName, target, activation);
}

function toFeedPage(dto: PageDto, byId: Record<string, Node>): FeedPage {
  const roots = flatten(dto.blocks, null, dto.name, byId, dto.format ?? "md");
  return {
    name: dto.name,
    kind: dto.kind,
    title: dto.title,
    preBlock: dto.pre_block,
    roots,
    format: dto.format ?? "md",
    readOnly: dto.read_only ?? false,
    guide: dto.guide ?? false,
    path: dto.path,
  };
}

/** Merge a page into the working set, replacing any prior copy of that page.
 *  Other loaded pages (and their nodes) are left untouched — so a page open in
 *  the sidebar survives navigating the main view elsewhere. */
function upsertPage(dto: PageDto): boolean {
  // A real page with this name exists again → lift any delete tombstone so edits
  // to the freshly-(re)created page save normally.
  untombstone(dto.name);
  const existing = doc.pages.find((p) => p.name === dto.name);
  // Self-write echo: the watcher re-reported our OWN just-saved content (Tine's
  // save normally suppresses this, but a synced/polled graph or a self-write-marker
  // gap can still surface it). A reload here rebuilds the page AND calls
  // invalidateUndoForPage, which would drop the undo entry we just pushed for the
  // edit that produced this exact content — that's the "delete a line, Ctrl+Z does
  // nothing" bug. If the incoming content is identical to what we already have, just
  // refresh the save baseline and keep the working copy + undo intact. A GENUINE
  // external change (content differs) still reloads + invalidates (data-safety #42).
  if (existing && pageContentMatches(dto, existing)) {
    setBaseRev(dto.name, dto.rev ?? null);
    return false;
  }
  // Replacing an already-loaded copy means the page's content changed under us
  // (a conflict-resolution / watcher reload). Any undo entry predating this reload
  // is stale — replaying it would clobber the just-loaded (external) version, so
  // drop those entries. (A first load has no prior entries → no-op.)
  const replacing = !!existing;
  // Activation retirement is deliberately NOT done here. Editable DTOs enter
  // through the two-phase installer below, which records B before compare-
  // retiring exact A. Retiring by name from this mutation primitive can race a
  // concurrent installer and destroy the activation it just installed.
  // Record the load baseline (the on-disk rev) so saves conflict against it.
  setBaseRev(dto.name, dto.rev ?? null);
  setDoc(
    produce((s) => {
      purgePageNodes(s, dto.name);
      const fp = toFeedPage(dto, s.byId);
      const i = s.pages.findIndex((p) => p.name === dto.name);
      if (i >= 0) s.pages[i] = fp;
      else s.pages.push(fp);
    })
  );
  activatePageInstance(dto.name);
  invalidateAllMatrixDimensions();
  if (replacing) invalidateUndoForPage(dto.name);
  return true;
}

/** Whether a reload DTO carries the SAME content (page-property pre-block + every
 *  block's raw + tree shape, ignoring block ids) as the page already in memory —
 *  i.e. a self-write echo, not a real external change. Lets `upsertPage` skip a
 *  needless reload that would otherwise reset block identities and invalidate the
 *  undo history for content we already hold. */
function pageContentMatches(dto: PageDto, page: FeedPage): boolean {
  if ((dto.path ?? "") !== (page.path ?? "")) return false;
  if ((dto.pre_block ?? null) !== (page.preBlock ?? null)) return false;
  const eq = (b: BlockDto, id: string): boolean => {
    const n = doc.byId[id];
    if (!n || n.raw !== b.raw || n.children.length !== b.children.length) return false;
    return b.children.every((cb, i) => eq(cb, n.children[i]));
  };
  return dto.blocks.length === page.roots.length && dto.blocks.every((b, i) => eq(b, page.roots[i]));
}

type CapturedEditorInstance = {
  generation: number;
  path?: string;
  kind: PageKind;
  activation?: number;
  activationTarget?: string;
};

function captureEditorInstance(name: string): CapturedEditorInstance | null {
  const page = pageByName(name);
  const generation = pageInstanceGeneration(name);
  if (!page || generation === null) return null;
  return {
    generation,
    path: page.path,
    kind: page.kind,
    activation: editorActivationFor(name),
    activationTarget: page.path || prospectiveTargets.get(name),
  };
}

function isExactCapturedInstance(name: string, captured: CapturedEditorInstance | null): boolean {
  const current = pageByName(name);
  if (!captured) return !current && peekPageInstanceGeneration(name) === undefined;
  return !!current
    && current.kind === captured.kind
    && current.path === captured.path
    && peekPageInstanceGeneration(name) === captured.generation
    && editorActivationFor(name) === captured.activation
    && (current.path || prospectiveTargets.get(name)) === captured.activationTarget;
}

export type EditorInstallOptions = {
  /** Binding captured before the read that produced the DTO. */
  expectedGraphBinding?: number;
  /** Explicit user-authorised discard; identity and binding still re-check. */
  bypassReplacementGate?: boolean;
  /** Surface/component ownership spanning the activation await. */
  isRequestLive?: () => boolean;
  /** Awaited only after disk read and replacement activation have succeeded.
   * Returning false compare-retires the minted activation without installing. */
  beforeInstall?: () => Promise<boolean>;
};

/** Load a page into the editable working set through the activation boundary.
 *
 * Replacement is a two-phase protocol: capture exact A under the synchronous
 * full gate; await B's activation; re-check both the gate and exact A; install B
 * and record it; then compare-retire A. A failed/stale activation never installs
 * an editable DTO. Same-instance same-content hydration stays idempotent. */
export async function ensurePageLoaded(
  dto: PageDto,
  options: EditorInstallOptions = {},
): Promise<InstanceRefusal | null> {
  const binding = options.expectedGraphBinding ?? graphBinding();
  if (binding !== graphBinding() || options.isRequestLive?.() === false) {
    return { reason: "stale-instance", page: dto.name };
  }

  const captured = captureEditorInstance(dto.name);
  const incumbent = pageByName(dto.name);
  const samePath = !!incumbent && (incumbent.path ?? "") === (dto.path ?? "");
  const sameInstanceHydration = !!incumbent && samePath && pageContentMatches(dto, incumbent);
  const replacing = !!captured && !sameInstanceHydration;

  // Phase 1: the complete synchronous gate, before activation. A same-instance
  // hydration does not replace anything, so a dirty editor may safely acquire
  // the identity it already owns.
  if (replacing && !options.bypassReplacementGate && !mayReplaceInstance(dto.name)) {
    return { reason: "unsaved-changes", page: dto.name };
  }

  // Already active exact-instance hydration is the idempotent fast path.
  if (sameInstanceHydration && editorActivationFor(dto.name) !== undefined) {
    // Exact same-content hydration still carries new disk authority. Skipping
    // this adoption after Concord Apply left the winner open with no baseline,
    // so the next ordinary edit correctly looked like a brand-new conflict.
    setBaseRev(dto.name, dto.rev ?? null);
    return null;
  }

  let handle: EditorActivationHandle | null = null;
  const editable = !dto.read_only && !dto.guide;
  if (editable) {
    try {
      if (dto.path) {
        // Reuse is only valid when this exact frontend instance already owns the
        // activation, and that case returned through the fast path above.  With
        // no local activation, mint a replacement even for a first installation:
        // a best-effort retirement from an older destroyed editor may have failed,
        // and inheriting that stale core record would cross editor episodes.
        const intent: ActivationIntent = "replace";
        handle = await backend().activateEditor(dto.path, intent, dto.rev ?? null);
      } else {
        handle = await backend().activateAbsentEditor(dto.name, dto.kind);
      }
    } catch {
      return { reason: "activation-failed", page: dto.name };
    }
  }

  // Presentation spends one-shot conflict authority, so identity must be
  // re-checked after activation and BEFORE that fallible/consuming operation.
  // The same check still runs again below after presentation, closing changes
  // that race the presentation await itself.
  if (
    binding !== graphBinding()
    || options.isRequestLive?.() === false
    || !isExactCapturedInstance(dto.name, captured)
  ) {
    await retireMintedActivation(handle);
    return { reason: "stale-instance", page: pageByName(dto.name)?.name ?? dto.name };
  }

  if (options.beforeInstall) {
    let proceed = false;
    try {
      proceed = await options.beforeInstall();
    } catch {
      proceed = false;
    }
    if (!proceed) {
      await retireMintedActivation(handle);
      return { reason: "stale-instance", page: pageByName(dto.name)?.name ?? dto.name };
    }
  }

  // Phase 3: both graph ownership and exact incumbent identity must survive the
  // await. Then re-evaluate the full gate in the same synchronous turn as the
  // install. A raced B is compare-retired; A/current remains untouched.
  if (
    binding !== graphBinding()
    || options.isRequestLive?.() === false
    || !isExactCapturedInstance(dto.name, captured)
  ) {
    await retireMintedActivation(handle);
    return { reason: "stale-instance", page: pageByName(dto.name)?.name ?? dto.name };
  }
  if (replacing && !options.bypassReplacementGate && !mayReplaceInstance(dto.name)) {
    await retireMintedActivation(handle);
    return { reason: "unsaved-changes", page: dto.name };
  }

  if (sameInstanceHydration) {
    setBaseRev(dto.name, dto.rev ?? null);
    if (handle) recordEditorActivation(dto.name, handle);
    return null;
  }

  // Phase 4: publish B's instance first, then its identity, then retire exact A.
  // `clearEditorActivation` is compare-based, so retiring A cannot clear B.
  upsertPage(dto);
  if (handle) recordEditorActivation(dto.name, handle);
  if (captured?.activation !== undefined) {
    await retireExactEditorActivation(
      dto.name,
      captured.activationTarget,
      captured.activation,
    );
  }
  evictIfNeeded();
  return null;
}

// Cap the working set so a long session browsing a big graph doesn't grow byId
// without bound. FIFO-evict pages that aren't pinned: the main feed, anything
// open in the right sidebar, the page being edited, and any page with unsaved
// edits are all kept (evicting a dirty page would lose those edits).
const WORKING_SET_CAP = 80;
let paneRouteProvider: () => Route[] = () => [];
export function registerPaneRouteProvider(provider: () => Route[]) {
  paneRouteProvider = provider;
}
function pinnedPages(): Set<string> {
  const pin = new Set<string>(doc.feed);
  for (const r of paneRouteProvider()) {
    if (r.kind === "page") pin.add(r.name);
  }
  for (const it of rightSidebar()) pin.add(it.kind === "page" ? it.name : it.page);
  for (const name of dirtyPages()) pin.add(name);
  // Conflicted pages hold unsaved edits that aren't in `dirty` (the save batch
  // removed them); evicting one would silently drop those edits.
  for (const name of conflicts()) pin.add(name);
  // A page whose save is in flight is ALSO not in `dirty` — `doSave` removes it
  // before awaiting the backend. Evicting it there loses the edit outright, and
  // if that save then fails transiently `doSave` re-adds a name with no page
  // behind it, which `pageToDto` cannot serialize: the name is stuck in `dirty`
  // forever and `flushAll()` can never succeed again. (Direct Files data-safety
  // audit, 2026-08-09, finding 6.)
  for (const name of savingPages()) pin.add(name);
  const ed = editingId();
  if (ed && doc.byId[ed]) pin.add(doc.byId[ed].page);
  return pin;
}

/** Pages whose current bytes matter before the focus freshness barrier can
 * release input. This deliberately exposes only the bounded working-set pins,
 * never the graph inventory: focus verification must stay O(visible/active
 * pages), not O(graph). */
export function focusFreshnessPageNames(): string[] {
  return [...pinnedPages()];
}
function evictIfNeeded() {
  if (doc.pages.length <= WORKING_SET_CAP) return;
  const pin = pinnedPages();
  const evicted: { name: string; path?: string }[] = [];
  setDoc(
    produce((s) => {
      // Oldest first (insertion order); stop once at the cap or only pinned left.
      for (let i = 0; i < s.pages.length && s.pages.length > WORKING_SET_CAP; ) {
        const name = s.pages[i].name;
        if (pin.has(name)) {
          i++;
          continue;
        }
        // Capture the path BEFORE the page leaves the working set. A retirement
        // that has to look the page up afterwards finds nothing and silently
        // retires nothing, leaking the native activation — which the next editor
        // of that path would then inherit under same-path Reuse.
        evicted.push({ name, path: s.pages[i].path });
        purgePageNodes(s, name);
        s.pages.splice(i, 1);
      }
    })
  );
  for (const { name, path } of evicted) {
    retireEditorFor(name, path);
    retirePageInstance(name);
  }
  invalidateAllMatrixDimensions();
}

/** Why a replacement was refused, for the surface that asked for it. */
export type InstanceRefusal = {
  reason: "unsaved-changes" | "stale-instance" | "activation-failed";
  /** The page holding the unsaved work — what the surface tells the user. */
  page: string;
};
export { editorActivations, evictIfNeeded, prospectiveTargets, upsertPage };
