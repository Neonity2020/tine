import type { Format, PageKind } from "../types";
import { backend } from "../backend";
import { blockWritable, clearReplaceableWatchers, doc, formatForBlock, markDirty, onPageBecameReplaceable, pageByName, setDoc } from "./doc";
import { ensurePageLoaded } from "./lifecycle";
import { flushPage, graphBinding, isTombstonedFile, tombstoneCovers } from "../persistence";


/** The block's existing durable `id` — a markdown `id:: <uuid>` trailer or an
 *  org `:PROPERTIES:` drawer `:id: <uuid>` line — case-insensitively, or null.
 *  Format-aware because in ORG `id:: x` is plain body text, NOT a property (lsdoc
 *  reads the drawer, not a `key::` line); so an org block's real id lives in its
 *  `:PROPERTIES:` drawer and must be matched there (GH #25). */
export function existingBlockId(raw: string, format: Format): string | null {
  const re = format === "org" ? /(?:^|\n):id:\s*(\S+)/i : /(?:^|\n)id:: *(\S+)/i;
  const m = re.exec(raw);
  return m ? m[1] : null;
}

/** The identity other blocks and persisted UI state must use for a loaded node.
 * A freshly-created node keeps its transient `b…` store key for the whole live
 * session even after Copy block ref writes a UUID property into `raw`; external
 * references must follow that property while render/edit paths keep the key. */
export function blockExternalId(id: string): string | null {
  const node = doc.byId[id];
  if (!node) return null;
  return existingBlockId(node.raw, formatForBlock(id)) ?? node.id;
}

export interface LoadedBlockRef {
  uuid: string;
  page: string;
  pageKind: PageKind;
  path?: string;
}

/** Resolve a durable external UUID back to the current live store key. The page
 * descriptor is part of the identity. Authored `id::`/`:id:` claims take
 * precedence over UUID-shaped runtime locators: after structural edits, a
 * locator can be reused by another sibling while the authored ID stays with the
 * intended block. Ambiguous authored claims fail closed; an ID-less runtime key
 * is only a fallback when no authored block claims the UUID (GH #373). */
export function resolveBlockRef(ref: LoadedBlockRef): string | null {
  const owner = pageByName(ref.page);
  if (
    !owner
    || owner.kind !== ref.pageKind
    || (ref.path !== undefined && owner.path !== ref.path)
  ) return null;

  const stack = [...owner.roots];
  const seen = new Set<string>();
  let authoredClaim: string | null = null;
  while (stack.length) {
    const id = stack.pop()!;
    if (seen.has(id)) continue;
    seen.add(id);
    const node = doc.byId[id];
    if (!node || node.page !== ref.page) continue;
    if (existingBlockId(node.raw, formatForBlock(id)) === ref.uuid) {
      // A second authored claimant is ambiguous. Never guess, and never rewrite
      // either block merely because a route exposed the conflict.
      if (authoredClaim !== null) return null;
      authoredClaim = id;
    }
    stack.push(...node.children);
  }
  if (authoredClaim !== null) return authoredClaim;

  // Structural/runtime locators are a compatibility fallback, not durable
  // external identity. Once a block has any authored ID, its runtime key must
  // not also resolve as a second identity.
  const runtime = doc.byId[ref.uuid];
  return runtime
    && runtime.page === ref.page
    && existingBlockId(runtime.raw, formatForBlock(ref.uuid)) === null
    ? ref.uuid
    : null;
}

/** `raw` with a durable `id` property added in the page's on-disk format.
 *  Markdown appends an `id:: <uuid>` trailer. ORG inserts/extends a
 *  `:PROPERTIES:`/`:id:`/`:END:` drawer at OG's canonical position — right after
 *  the title line and any SCHEDULED/DEADLINE planning lines (mirroring OG's
 *  `insert-property`, util/property.cljs). Writing markdown `id::` into an org
 *  file would BOTH render as visible body text and not be read back as the
 *  block's id (GH #25) — org MUST use the drawer. The caller guarantees the
 *  block has no id yet (see {@link existingBlockId}). */
export function rawWithBlockId(raw: string, uuid: string, format: Format): string {
  if (format !== "org") return `${raw}\nid:: ${uuid}`;
  const lines = raw.split("\n");
  const start = lines.findIndex((l) => l.trim().toUpperCase() === ":PROPERTIES:");
  const end =
    start >= 0 ? lines.findIndex((l, i) => i > start && l.trim().toUpperCase() === ":END:") : -1;
  if (start >= 0 && end > start) {
    // Extend the existing drawer: insert the id line just before :END:.
    lines.splice(end, 0, `:id: ${uuid}`);
    return lines.join("\n");
  }
  // No drawer: title, SCHEDULED*, DEADLINE*, :PROPERTIES: drawer, rest-of-body —
  // OG groups planning lines above the drawer (util/property.cljs insert-property).
  const [title, ...rest] = lines;
  const isSched = (l: string) => l.startsWith("SCHEDULED");
  const isDead = (l: string) => l.startsWith("DEADLINE");
  const scheduled = rest.filter(isSched);
  const deadline = rest.filter(isDead);
  const body = rest.filter((l) => !isSched(l) && !isDead(l));
  return [title, ...scheduled, ...deadline, ":PROPERTIES:", `:id: ${uuid}`, ":END:", ...body].join(
    "\n"
  );
}

/** Ensure a block has a persistent id (assigned lazily, like OG) AND that it's
 *  durably on disk, returning the uuid — or null if it couldn't be saved
 *  (conflict/error). Used to make `((uuid))` references: the caller must not put
 *  a ref on the clipboard until the id is actually written, or quitting /
 *  resolving a conflict with "use disk version" would leave the ref dangling. */
export async function ensureBlockId(id: string): Promise<string | null> {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return null;
  const fmt = formatForBlock(id);
  // Any existing id is the block's durable id — match its value (not just a UUID
  // shape), case-INSENSITIVELY (Rust's property("id") is case-insensitive, so an
  // `ID::` / `:ID:` from another editor counts), so we never write a SECOND id
  // that Rust then ignores → dangling copied ref.
  const existing = existingBlockId(node.raw, fmt);
  const uuid = existing ?? crypto.randomUUID();
  if (!existing) {
    setDoc("byId", id, "raw", rawWithBlockId(node.raw, uuid, fmt));
    markDirty(node.page);
  }
  // Even a pre-existing id may not be on disk yet (added in-memory, not flushed);
  // flush and only hand back the uuid if the write actually landed.
  const ok = await flushPage(node.page);
  return ok ? uuid : null;
}

/** A live reference to a loaded block: its durable external UUID plus its exact
 * owner. The UUID can differ from the live store key until the page is reloaded. */
export function blockRef(id: string): LoadedBlockRef {
  const n = doc.byId[id];
  const owner = pageByName(n.page);
  return {
    uuid: blockExternalId(id) ?? n.id,
    page: n.page,
    pageKind: owner?.kind ?? "page",
    ...(owner?.path ? { path: owner.path } : {}),
  };
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Whether a block reference's id can name a block at all. OG resolves
 *  `((id))` only when `parse-uuid` accepts the id; `(((uuid)))` parses as the
 *  id `(uuid`, which it shows as an invalid reference (GH #589). */
export function isBlockRefUuid(id: string): boolean {
  return UUID_RE.test(id);
}

/** Ensure a block has a durable external UUID synchronously, while deliberately
 * leaving its live store key unchanged. Existing ids win; otherwise the block
 * always receives a fresh UUID in the page's Markdown/Org property syntax.
 * Runtime keys can themselves be deterministic UUIDs, but remain locators and
 * must never be persisted as authored identity (GH #373). */
export function ensureStableBlockId(id: string): string | null {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return null;
  const fmt = formatForBlock(id);
  const existing = existingBlockId(node.raw, fmt);
  if (existing) return existing;
  const uuid = crypto.randomUUID();
  setDoc("byId", id, "raw", rawWithBlockId(node.raw, uuid, fmt));
  markDirty(node.page);
  // Persist now, not on the 400ms debounce: the user may quit right after
  // parking the block, and a pending timer is lost when the webview closes.
  void flushPage(node.page);
  return uuid;
}

/** Stamp the exact external ID already committed by an inline `((uuid))`
 * reference. This is deliberately narrower than `ensureStableBlockId`: callers
 * choose the external value before committing the source text. At this boundary
 * that value is external identity even if it happens to equal the target DTO's
 * runtime locator. Deferred stamping must preserve it exactly or the
 * already-visible reference would dangle. */
function ensureCommittedBlockRefId(id: string, uuid: string): string | null {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return null;
  const fmt = formatForBlock(id);
  const existing = existingBlockId(node.raw, fmt);
  if (existing) return existing === uuid ? existing : null;
  setDoc("byId", id, "raw", rawWithBlockId(node.raw, uuid, fmt));
  markDirty(node.page);
  void flushPage(node.page);
  return uuid;
}

/** Like `blockRef`, but first persists the block's `id::` so the reference
 *  resolves after a restart. Used for parking a block durably: the right sidebar,
 *  a new tab, and zoom all stamp `id::` so the spot survives a relaunch (Martin's
 *  call — he wants these to persist; the `id::` is harmless in the file and is
 *  stripped from clipboard copies anyway, see `blockSubtreeMarkdown`). */
export function persistentBlockRef(id: string): LoadedBlockRef {
  ensureStableBlockId(id);
  return blockRef(id);
}

/** Make a freshly-inserted `((uuid))` reference durable: ensure the TARGET block
 *  (which may live on a page that isn't loaded — block search spans the whole
 *  graph) carries `id:: uuid` on disk, so the ref still resolves after a restart.
 *  The owning page is loaded only if absent (`ensurePageLoaded` never clobbers
 *  unsaved edits). A no-op if the block already has an `id::`. Fire-and-forget:
 *  the ref resolves in-session via the in-memory uuid even before this lands. */
export async function persistBlockRefTarget(
  uuid: string,
  page: string,
  kind: PageKind,
  path?: string,
): Promise<void> {
  const ref: LoadedBlockRef = { uuid, page, pageKind: kind, ...(path ? { path } : {}) };
  // The GRAPH BINDING, not the render epoch: toggling typography or the journal
  // format bumps the epoch without the graph moving, and dropping a committed
  // reference's request because the user changed a display preference is loss
  // with no safety benefit at all. (GH #254 increment 3, round 12.)
  const epoch = graphBinding();
  if (!resolveBlockRef(ref)) {
    const dto = path
      ? await backend().getPageByPath(path)
      : await backend().getPage(page, kind);
    // A read that crossed a graph switch must not install into the NEW graph.
    if (epoch !== graphBinding()) return;
    // Nor may one that crossed a DELETION. This read may have been issued before
    // the user deleted the page; installing its pre-delete bytes puts the page
    // back, and `upsertPage` lifts the tombstone as it does so, after which the
    // stamp's own save recreates the file the user just deleted — with stale
    // content. Routing deletion through the store exists precisely to stop a
    // queued write resurrecting a page, and this is the same hazard arriving by
    // a different door. (GH #254 increment 3.)
    // Path-aware, not name-level: two files legitimately share one page name, and
    // deleting one must not refuse the other. Refusing by name loses the surviving
    // owner's durable target — work lost rather than protected.
    if (isTombstonedFile(page, dto?.path ?? path)) {
      // RETAIN, don't discard. A tombstone is raised BEFORE the backend delete
      // and lifted again if that delete fails (an ambiguous by-name delete of a
      // duplicated page name is rejected by core). Dropping the request here
      // threw away an already-committed reference's durable target on a delete
      // that never happened. Retaining costs nothing: the retry re-checks the
      // tombstone before it reads, so while the page stays deleted this waits
      // silently, and it re-drives if the page comes back.
      retainStamp({ uuid, page, kind, path, epoch });
      return;
    }
    if (dto && await ensurePageLoaded(dto, { expectedGraphBinding: epoch })) {
      // RETAIN the request. The user-visible mutation has already happened —
      // autocomplete committed `((uuid))`, or the sidebar item is already open —
      // and this stamp is what makes those survive a restart. Skipping it leaves
      // a reference that resolves now and is gone after a restart; rolling it
      // back would undo what the user just typed.
      //
      // Driven by the "became replaceable" transition, NOT by polling on
      // unrelated saves: three poll-shaped designs were each reproduced failing,
      // and the liveness half is why — a request stranded whenever the incumbent
      // resolved through a route that produced no such save.
      // (GH #254 increment 3, acceptance row C5.)
      retainStamp({ uuid, page, kind, path, epoch });
      return;
    }
  }
  // Re-check: a concurrent navigation may have loaded the page meanwhile, or the
  // cache may have been rebuilt (external change) and reassigned the block a new
  // uuid — in which case there's nothing safe to stamp.
  const id = resolveBlockRef(ref);
  if (id) {
    pendingBlockRefStamps.delete(uuid);
    ensureCommittedBlockRefId(id, uuid);
  }
}

/** Stamps deferred by a refused replacement, keyed by the referenced uuid. */
const pendingBlockRefStamps = new Map<
  string,
  { uuid: string; page: string; kind: PageKind; path?: string; epoch: number }
>();

type PendingStamp = {
  uuid: string;
  page: string;
  kind: PageKind;
  path?: string;
  epoch: number;
};

/** Stop-handles for the armed watchers, so re-retaining one request replaces its
 *  watcher instead of stacking a second one that would re-read the same page. */
const stampWatchers = new Map<string, () => void>();

/** Is a deferred stamp still waiting? The distinction that matters is "retained"
 *  versus "dropped": a retained request will resume, a dropped one is work the
 *  user committed and silently lost. Nothing else can observe that difference. */
export function hasPendingBlockRefStamp(uuid: string): boolean {
  return pendingBlockRefStamps.has(uuid);
}

/** A retained stamp belongs to the graph that deferred it. */
export function clearPendingBlockRefStamps(): void {
  pendingBlockRefStamps.clear();
  stampWatchers.clear();
  clearReplaceableWatchers();
}

/** Hold a deferred stamp and (re-)arm exactly one watcher for it. */
function retainStamp(req: PendingStamp) {
  stampWatchers.get(req.uuid)?.();
  pendingBlockRefStamps.set(req.uuid, req);
  const stop = onPageBecameReplaceable(req.page, () => {
    // Stay armed and read nothing only when the tombstone PROVABLY covers this
    // request — which means the request itself names the deleted file. Anything
    // weaker is unsound: a request that cannot name its file must READ, because
    // nothing else can tell it the page came back. (Caching the file a previous
    // read found looks like a cheap way to skip that read, and is wrong: an
    // unloaded page recreated at a DIFFERENT path never upserts, so the
    // tombstone is never lifted and the cached path refuses forever. Re-reading
    // on each announcement is the price of not stranding the request.)
    if (tombstoneCovers(req.page, req.path)) return;
    stop();
    stampWatchers.delete(req.uuid);
    pendingBlockRefStamps.delete(req.uuid);
    if (req.epoch !== graphBinding()) return;
    void persistBlockRefTarget(req.uuid, req.page, req.kind, req.path);
  });
  stampWatchers.set(req.uuid, stop);
}
export { UUID_RE };
