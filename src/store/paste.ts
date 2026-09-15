import type { ClipboardBlock, ClipboardPayloadSlot } from "../clipboard";
import type { Format, PageDto, PageKind } from "../types";
import type { OutlineNode } from "../editor/outline";
import { Node, blockWritable, doc, formatForPage, freshId, hasLoadedIdentityCollision, markDirty, pageByName, pageInstanceGeneration, pageWritable, setDoc } from "./doc";
import { UUID_RE, existingBlockId } from "./blockRefs";
import { backend } from "../backend";
import { consumeCutGrant } from "../clipboard";
import { cutSourcePagesRetired, flushCutSourcePages, flushPage, graphBinding } from "../persistence";
import { dispatchBulkInsertion } from "../storageDispatch";
import { ensurePageLoaded } from "./lifecycle";
import { graphBindingRuntime } from "../graphBindingRuntime";
import { graphEpoch, graphMeta, graphTransitioning, pushToast } from "../ui";
import { hideAll, isBuiltinHidden, joinProps, splitProps } from "../editor/properties";
import { journalTitle } from "../journal";
import { parseOutline } from "../editor/outline";
import { produce, unwrap } from "solid-js/store";
import { pushUndo, withUndoUnit } from "./undo";
import { rawWithInheritedOrderListType } from "./properties";
import { recordClipboardPhaseForTest, recordClipboardWorkForTest } from "../clipboardWorkProbe";

let insertOutlineAfterImpl: ((afterId: string, nodes: OutlineNode[]) => string) | null = null;

export function registerInsertOutlineAfter(
  fn: (afterId: string, nodes: OutlineNode[]) => string,
): void {
  insertOutlineAfterImpl = fn;
}


export const BULK_INSERTION_UNAVAILABLE_TOAST =
  "Can't insert while the graph is changing. Nothing was changed.";

/** Result of the synchronous route decision a bulk insertion makes before any
 * await. Direct Files is the only authority; a refusal names its toast. */
export type BulkInsertionPreflight =
  | { kind: "direct" }
  | { kind: "refused"; toast: string };

/** Materialize a parsed quick-capture outline as the first roots of an empty
 * page. The caller owns one undo unit; no empty anchor is ever published. */
function insertCaptureOutlineIntoEmptyPage(pageName: string, nodes: OutlineNode[]): string | null {
  const page = pageByName(pageName);
  if (!page || page.roots.length || !nodes.length || !pageWritable(pageName)) return null;
  const format = formatForPage(pageName);
  let lastId: string | null = null;
  setDoc(produce((state) => {
    const create = (outline: OutlineNode, parent: string | null): string => {
      const id = freshId();
      const children = outline.children.map((child) => create(child, id));
      state.byId[id] = {
        id,
        raw: rawWithInheritedOrderListType(outline.raw, format, null),
        collapsed: false,
        parent,
        page: pageName,
        children,
      };
      return id;
    };
    const roots = nodes.map((node) => create(node, null));
    state.pages[state.pages.findIndex((candidate) => candidate.name === pageName)].roots.push(...roots);
    lastId = roots[roots.length - 1] ?? null;
  }));
  markDirty(pageName);
  return lastId;
}

type ClipboardProperty = { key: string; value: string };

function clipboardProperties(raw: string, format: Format): ClipboardProperty[] {
  const hidden = splitProps(raw, hideAll, format).hidden;
  if (!hidden) return [];
  const properties: ClipboardProperty[] = [];
  for (const line of hidden.split("\n")) {
    const match = format === "org"
      ? /^\s*:([A-Za-z0-9_@./-]+):\s*(.*)$/.exec(line)
      : /^\s*([A-Za-z0-9_./-]+)::\s*(.*)$/.exec(line);
    if (match) properties.push({ key: match[1], value: match[2] });
  }
  return properties;
}

function clipboardIdsForBlock(block: ClipboardBlock): string[] {
  return clipboardProperties(block.raw, block.sourceFormat)
    .filter((property) => property.key.toLowerCase() === "id")
    .map((property) => property.value.trim());
}

function clipboardRawForTarget(
  block: ClipboardBlock,
  targetFormat: Format,
  preserveIds: boolean,
): string {
  if (block.sourceFormat === targetFormat) {
    return preserveIds
      ? block.raw
      : splitProps(block.raw, (key) => key.toLowerCase() === "id", block.sourceFormat).visible;
  }

  // splitProps/joinProps classify metadata but deliberately do not translate
  // syntax. Map the ordered key/value stream explicitly so every property keeps
  // its relative order across Markdown `key:: value` and Org drawer forms.
  const visible = splitProps(block.raw, hideAll, block.sourceFormat).visible;
  const properties = clipboardProperties(block.raw, block.sourceFormat)
    .filter((property) => preserveIds || property.key.toLowerCase() !== "id");
  const translated = properties.map(({ key, value }) =>
    targetFormat === "org" ? `:${key}: ${value}` : `${key}:: ${value}`
  ).join("\n");
  return joinProps(visible, translated, targetFormat);
}

function clipboardCollapsed(block: ClipboardBlock): boolean {
  return clipboardProperties(block.raw, block.sourceFormat)
    .some(({ key, value }) => key.toLowerCase() === "collapsed" && value.trim().toLowerCase() === "true");
}

function liveDocReferences(id: string): boolean {
  const escaped = id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const reference = new RegExp(`\\(\\(${escaped}\\)\\)`, "i");
  return Object.values(doc.byId).some((node) => reference.test(node.raw));
}

/** What a bulk insertion route decided against, so a continuation resuming after
 * an await can prove it is still talking about the same block, the same page
 * instance, the same graph AND the same storage route.
 *
 * The storage route is part of it because a route is selected synchronously and
 * acted on later: a drop or paste can still be reading a file while the graph
 * binding changes underneath it, and would then insert against a binding it was
 * never admitted to (GH #325). Graph activation flips the binding inside its
 * own transitioning window, so `graphTransitioning` alone stops only
 * continuations that resume DURING the switch, not the ones that resume just
 * after it. */
export interface BulkRouteFence {
  epoch: number;
  root: string;
  targetId: string;
  targetNode: Node;
  targetPage: string;
  targetGeneration: number;
  routeBindingGeneration: number | null;
}

export function captureBulkRouteFence(targetId: string): BulkRouteFence | null {
  const target = doc.byId[targetId];
  if (!target || graphTransitioning()) return null;
  const targetGeneration = pageInstanceGeneration(target.page);
  if (targetGeneration === null) return null;
  const route = graphBindingRuntime.snapshot().applicationPageAdmission;
  return {
    epoch: graphEpoch(),
    root: graphMeta()?.root ?? "",
    targetId,
    targetNode: unwrap(target),
    targetPage: target.page,
    targetGeneration,
    routeBindingGeneration: route?.binding_generation ?? null,
  };
}

export function bulkRouteFenceCurrent(fence: BulkRouteFence): boolean {
  const target = doc.byId[fence.targetId];
  const route = graphBindingRuntime.snapshot().applicationPageAdmission;
  return !graphTransitioning()
    && graphEpoch() === fence.epoch
    && (graphMeta()?.root ?? "") === fence.root
    && !!target
    && unwrap(target) === fence.targetNode
    && target.page === fence.targetPage
    && pageInstanceGeneration(fence.targetPage) === fence.targetGeneration
    && (route?.binding_generation ?? null) === fence.routeBindingGeneration;
}

function clipboardTargetReusesEmptyHost(target: Node, targetFormat: Format): boolean {
  const visible = splitProps(target.raw, isBuiltinHidden, targetFormat).visible;
  return target.children.length === 0
    && visible.trim() === ""
    && existingBlockId(target.raw, targetFormat) === null
    && !liveDocReferences(target.id);
}

function insertClipboardBlocksSync(
  targetId: string,
  blocks: readonly ClipboardBlock[],
  preserveIds: boolean,
  preservedIds: readonly string[],
  reuseEmptyHost?: boolean,
): string | null {
  const target = doc.byId[targetId];
  if (!blocks.length || !target || !blockWritable(targetId)) return null;
  const targetFormat = formatForPage(target.page);
  const prepared = blocks.map(function prepare(block): {
    id: string;
    raw: string;
    collapsed: boolean;
    children: ReturnType<typeof prepare>[];
  } {
    if (import.meta.env.MODE === "test") recordClipboardWorkForTest("prepared_destination_nodes");
    const sourceIds = clipboardIdsForBlock(block);
    return {
      id: preserveIds && sourceIds.length === 1 ? sourceIds[0].toLowerCase() : freshId(),
      raw: clipboardRawForTarget(block, targetFormat, preserveIds),
      collapsed: clipboardCollapsed(block),
      children: block.children.map(prepare),
    };
  });
  const replaceHost = reuseEmptyHost ?? clipboardTargetReusesEmptyHost(target, targetFormat);
  const parent = target.parent;
  const pageName = target.page;
  let lastId: string | null = null;

  if (import.meta.env.MODE === "test") {
    recordClipboardWorkForTest("target_insertion_phases");
    recordClipboardPhaseForTest("target-insertion");
  }
  pushUndo("clipboard-paste", [pageName], preserveIds ? preservedIds : []);
  setDoc(produce((state) => {
    const create = (block: typeof prepared[number], blockParent: string | null): string => {
      if (import.meta.env.MODE === "test") recordClipboardWorkForTest("allocated_destination_nodes");
      const children = block.children.map((child) => create(child, block.id));
      state.byId[block.id] = {
        id: block.id,
        raw: block.raw,
        collapsed: block.collapsed,
        parent: blockParent,
        page: pageName,
        children,
      };
      return block.id;
    };
    const created = prepared.map((block) => create(block, parent));
    const siblings = parent === null
      ? state.pages[state.pages.findIndex((page) => page.name === pageName)].roots
      : state.byId[parent].children;
    const at = siblings.indexOf(targetId);
    if (replaceHost) {
      siblings.splice(at, 1, ...created);
      delete state.byId[targetId];
    } else {
      siblings.splice(at + 1, 0, ...created);
    }
    lastId = created[created.length - 1] ?? null;
  }));
  markDirty(pageName);
  return lastId;
}

/** Associate an already-captured private clipboard slot with one target. The
 * wrapper is intentionally non-async: a cut grant is consumed synchronously,
 * before the returned continuation can reach retirement or any other await. */
export function pasteClipboardPayload(
  targetId: string,
  slot: ClipboardPayloadSlot,
): Promise<string | null> {
  const authority = captureBulkRouteFence(targetId);
  if (!authority) return Promise.resolve(null);

  // Plan as if the host will NOT be reused. Whether an empty host is replaced is
  // decided from the live block at insertion time, after this route's awaits, so
  // no cached answer may authorize deleting a block the user has since typed
  // into (GH #322). Assuming the host survives keeps the admitted block count an
  // upper bound on the realized one either way; the cost is refusing a paste
  // into an empty host on a page already exactly at the limit.
  const admission = dispatchBulkInsertion<BulkInsertionPreflight>(
    { targetId },
    {
      direct: () => ({ kind: "direct" }),
      unavailable: () => ({ kind: "refused", toast: BULK_INSERTION_UNAVAILABLE_TOAST }),
    },
  );
  if (admission.kind === "refused") {
    pushToast(admission.toast, "error");
    return Promise.resolve(null);
  }

  // This must remain after the initial synchronous admission: a known target
  // overflow leaves a Cut grant intact for a smaller retry.
  const grant = slot.op === "cut" ? consumeCutGrant(slot.generation) : null;

  const idLists: string[][] = [];
  const visit = (block: ClipboardBlock) => {
    idLists.push(clipboardIdsForBlock(block));
    block.children.forEach(visit);
  };
  slot.blocks.forEach(visit);
  const ids = idLists.flat();
  const normalizedIds = ids.map((id) => id.toLowerCase());
  const idsValid = idLists.every((blockIds) => blockIds.length <= 1)
    && ids.every((id) => UUID_RE.test(id))
    && new Set(normalizedIds).size === normalizedIds.length;

  return (async () => {
    let preserveIds = !!grant
      && ids.length > 0
      && idsValid
      && slot.graph === authority.root;

    if (preserveIds) {
      if (import.meta.env.MODE === "test") {
        recordClipboardWorkForTest("source_retirement_phases");
        recordClipboardPhaseForTest("source-retirement");
      }
      preserveIds = await flushCutSourcePages(grant!.sourcePages);
      if (preserveIds && !bulkRouteFenceCurrent(authority)) return null;
    }
    if (preserveIds) {
      try {
        if (import.meta.env.MODE === "test") {
          recordClipboardWorkForTest("resolve_blocks_phases");
          recordClipboardPhaseForTest("resolve-blocks");
        }
        const resolved = await backend().resolveBlocks(normalizedIds);
        preserveIds = resolved.length === normalizedIds.length && resolved.every((block) => block === null);
      } catch {
        preserveIds = false;
      }
    }

    // Final JS-single-thread section: every authority and retirement check is
    // synchronous and insertion follows immediately with no await boundary.
    if (!bulkRouteFenceCurrent(authority)) return null;
    if (preserveIds) {
      if (import.meta.env.MODE === "test") {
        recordClipboardWorkForTest("final_identity_guard_phases");
        recordClipboardPhaseForTest("final-identity-guard");
      }
      preserveIds = cutSourcePagesRetired(grant!.sourcePages)
        && !hasLoadedIdentityCollision(normalizedIds);
    }
    const reuseEmptyHost = clipboardTargetReusesEmptyHost(
      doc.byId[targetId],
      formatForPage(doc.byId[targetId].page),
    );
    return insertClipboardBlocksSync(
      targetId,
      slot.blocks,
      preserveIds,
      preserveIds ? normalizedIds : [],
      reuseEmptyHost,
    );
  })();
}

/** Append a quick-capture (Logseq outline markdown, as produced by the capture
 *  window's editor — usually one bullet, but templates/multi-line paste can make
 *  several) at the END of today's journal, then flush immediately. This is the
 *  single writer for global quick-capture: routing through the live store (rather
 *  than a separate-process file append) means a capture can't race a main-view
 *  edit of today's journal into a conflict. Loads — or, if the day has no file
 *  yet, synthesizes — the journal first; never clobbers in-progress edits
 *  (`ensurePageLoaded` is a no-op when already loaded). Returns whether the write
 *  reached disk. */
export async function appendToTodayJournal(markdown: string): Promise<boolean> {
  return captureOutlineInto(journalTitle(new Date()), "journal", parseOutline(markdown));
}

/** In-app quick capture into a (new or existing) named PAGE — the heading-filled
 *  branch of the journal-top capture bar. Same single-writer guarantees as
 *  {@link appendToTodayJournal}: routes through the live store + immediate flush,
 *  so it can't race a main-view edit of the same page into a conflict. */
export async function captureToPage(title: string, markdown: string): Promise<boolean> {
  const name = title.trim();
  if (!name) return false;
  return captureOutlineInto(name, "page", parseOutline(markdown));
}

/** Append outline `nodes` at the END of the named page (loaded — or synthesized
 *  if it has no file yet — first), then flush immediately. Shared by the journal
 *  append and the new-page capture; never clobbers in-progress edits
 *  (`ensurePageLoaded` is a no-op when already loaded). Returns whether it landed. */
async function captureOutlineInto(name: string, kind: PageKind, nodes: OutlineNode[]): Promise<boolean> {
  if (!nodes.length) return false;
  if (!pageByName(name)) {
    const binding = graphBinding();
    const dto: PageDto =
      (await backend().getPage(name, kind)) ??
      { name, kind, title: name, pre_block: null, blocks: [], rev: null };
    // Stop on a refusal rather than falling through to `pageByName` for the name
    // slot: that would append the capture into whichever editor is loaded under
    // this name, which on a refusal is a DIFFERENT file. Returning false keeps
    // the capture text where the caller can retry it. (GH #254 increment 3.)
    if (await ensurePageLoaded(dto, { expectedGraphBinding: binding })) return false;
  }
  const page = pageByName(name);
  if (!page || !pageWritable(name)) return false;
  const insertionTarget = page.roots.length ? page.roots[page.roots.length - 1] : null;
  const admission = dispatchBulkInsertion<BulkInsertionPreflight>(
    { targetId: insertionTarget, targetPageName: name },
    {
      direct: () => ({ kind: "direct" }),
      unavailable: () => ({ kind: "refused", toast: BULK_INSERTION_UNAVAILABLE_TOAST }),
    },
  );
  if (admission.kind === "refused") {
    pushToast(admission.toast, "error");
    return false;
  }
  if (page.roots.length) {
    // Append after the last top-level block (end of the page).
    insertOutlineAfterImpl!(insertionTarget!, nodes);
  } else {
    withUndoUnit("capture", [name], () => insertCaptureOutlineIntoEmptyPage(name, nodes));
  }
  return await flushPage(name);
}
