import type { BlockDto, PageDto } from "../types";
import type { OutlineNode } from "../editor/outline";
import { FeedPage, Node, doc, editGeneration, editorTransactionGeneration, freshId, markDirty, pageByName, pageInstanceGeneration, pageWritable, setDoc } from "./doc";
import { cloneNode, clonePages, pushUndo } from "./undo";
import { editorActivations, prospectiveTargets } from "./lifecycle";
import { graphBinding, saveBaselineFor } from "../persistence";
import { graphBindingRuntime } from "../graphBindingRuntime";
import { graphEpoch, graphMeta, graphTransitioning, pushToast } from "../ui";
import { isPageHeaderPropertiesOnly, markdownRawWithProperty, orgRawWithProperty, parsePageHeaderPropertyLine } from "../editor/properties";
import { produce, unwrap } from "solid-js/store";
import { PAGE_HEADER_INVALID_TOAST, rawWithInheritedOrderListType } from "./properties";
import { trimBlockTrailingSpace } from "../editor/format";


export function __setPageMutationEffectFailureForTest(enabled: boolean): void {
  if (import.meta.env.MODE !== "test") return;
  pageMutationEffectFailureForTest = enabled;
}

function toDtoFrom(nodes: Readonly<Record<string, Node>>, id: string): BlockDto {
  const n = nodes[id];
  // Trim a block's trailing space only here, at the disk-write boundary — OG
  // keeps the space while you edit and trims on save. (The live editor buffer
  // keeps it so backspacing to a trailing space doesn't eat the space out from
  // under the caret.) `trimBlockTrailingSpace` is idempotent and only touches
  // whitespace at the very end of the block, so a block with nothing to trim
  // serializes byte-identically — no churn, no property reordering.
  return {
    id: n.id,
    raw: trimBlockTrailingSpace(n.raw),
    collapsed: n.collapsed,
    children: n.children.map((child) => toDtoFrom(nodes, child)),
  };
}

function toDto(id: string): BlockDto {
  return toDtoFrom(doc.byId, id);
}

/** Properties that describe the block they sit on, never a page: a first
 *  bullet carrying one stays an outline block. An empty numbered-list item is
 *  exactly `logseq.order-list-type:: number`, and folding it into the page
 *  header turned the list into page properties and jammed every later save
 *  (GH #540). Mirror of Rust `BLOCK_SCOPED_PROPERTY_KEYS` (model/page_header.rs),
 *  which carries the OG provenance. */
export const BLOCK_SCOPED_PROPERTY_KEYS: readonly string[] = [
  "id",
  "heading",
  "collapsed",
  "background-color",
  "logseq.order-list-type",
];

/** Mirror of Rust `first_root_is_promotable_page_header` (model/page_header.rs):
 *  a childless first root whose raw is exactly canonical page-header properties
 *  and carries no block-scoped property (an `id::` block is a real referenced
 *  outline block, an empty numbered item a list item; the Rust promote
 *  branch/firewall both leave them as bullets). */
function isPromotablePageHeaderRoot(node: Node): boolean {
  const canonicalRaw = node.raw.replace(/\n+$/, "");
  return (
    node.children.length === 0 &&
    isPageHeaderPropertiesOnly(canonicalRaw) &&
    !canonicalRaw.split("\n").some((line) => {
      const key = parsePageHeaderPropertyLine(line)?.key.toLowerCase();
      return key !== undefined && BLOCK_SCOPED_PROPERTY_KEYS.includes(key);
    })
  );
}

function projectPageDto(
  p: FeedPage | undefined,
  nodes: Readonly<Record<string, Node>>,
  reportInvalidHeader: boolean,
): PageDto | null {
  if (!p) return null;
  let rootIds = p.roots;
  let preBlock = p.preBlock;
  const first = nodes[rootIds[0]];
  if (first?.originatedFromPageHeader) {
    // Enter temporarily leaves one or more trailing newlines in the live
    // page-header editor. Tolerate only that authoring artifact at the disk
    // firewall; keep the strict shared display predicate and live raw intact.
    const canonicalRaw = first.raw.replace(/\n+$/, "");
    if (first.children.length > 0 || (first.raw !== "" && !isPageHeaderPropertiesOnly(canonicalRaw))) {
      if (reportInvalidHeader) {
        pushToast(PAGE_HEADER_INVALID_TOAST, "error");
      }
      return null;
    }
    // Exact raw is authoritative here: ordinary toDto trimming must never eat a
    // page-header value or its separator trivia. An empty draft deletes the
    // header and emits no stray outline bullet.
    preBlock = canonicalRaw ? canonicalRaw + (p.preBlock ?? "") : p.preBlock;
    rootIds = rootIds.slice(1);
  } else if (first && !p.preBlock && isPromotablePageHeaderRoot(first)) {
    // GH #198: a flagless "properties-only first bullet" (empty preBlock) IS the
    // page header — the same shape setPageProperty/beginPageHeaderEdit already
    // treat as the header. Fold it into pre_block so the DTO is honest, instead
    // of leaning on the Rust promote branch: once disk already carries the
    // promoted preamble, the GH #163 preservation firewall refuses the
    // pre_block=None + first-root-properties DTO and jams the save queue with a
    // "will retry" toast forever. Folding here emits pre_block=properties, so
    // the firewall precondition (empty pre_block) is false and the save writes
    // the identical canonical preamble. Mirrors Rust's promotability rule.
    preBlock = first.raw.replace(/\n+$/, "");
    rootIds = rootIds.slice(1);
  }
  let blocks = rootIds.map((id) => toDtoFrom(nodes, id));
  // Don't persist a lone placeholder block. A page that exists only for its
  // properties is loaded with one empty editable bullet (toLoadable); saving it
  // — e.g. after a page-property edit — must NOT write that bullet back as a
  // stray "- " and corrupt the round-trip. Symmetric with the load side;
  // reopening re-adds the editable bullet.
  if (blocks.length === 1 && blocks[0].raw.trim() === "" && blocks[0].children.length === 0) {
    blocks = [];
  }
  return {
    name: p.name,
    kind: p.kind,
    title: p.title,
    pre_block: preBlock,
    blocks,
    format: p.format,
    // Which live editor is issuing this save. Read from the registry rather than
    // carried on the page, so no clone or history snapshot can claim it.
    // (GH #254 increment 3.)
    activation: editorActivations.get(p.name),
    // Pin the save to the exact file this page came from (#21). For an editor
    // activated with no file yet, this is the prospective target it is live for —
    // without it the DTO goes out unpinned and the core cannot recognise its own
    // absent editor when the target drifts underneath it.
    path: p.path || prospectiveTargets.get(p.name) || "",
    guide: p.guide,
    read_only: p.readOnly,
  };
}

// ---------------------------------------------------------------------------
// Detached one-page mutation plans
// ---------------------------------------------------------------------------

export type PageMutationEffect =
  | { kind: "create"; node: Readonly<Omit<Node, "children">> & { readonly children: readonly string[] }; parent: string; at: number }
  | { kind: "delete"; id: string }
  | { kind: "raw"; id: string; raw: string }
  | { kind: "property"; id: string; key: string; value: string | null; raw: string }
  | { kind: "parent"; id: string; parent: string }
  | { kind: "order"; parent: string; children: readonly string[] };

export type PageMutationDraftNode = Readonly<Omit<Node, "children">> & {
  readonly children: readonly string[];
};

export type PageMutationDraftPage = Readonly<Omit<FeedPage, "roots">> & {
  readonly roots: readonly string[];
};

export interface PageMutationDraft {
  readonly page: PageMutationDraftPage;
  node(id: string): PageMutationDraftNode | undefined;
  createChild(parentId: string, at: number, raw?: string): string | null;
  insertOutlineChildren(parentId: string, outlines: readonly OutlineNode[]): string | null;
  deleteSubtree(id: string): boolean;
  setRaw(id: string, raw: string): boolean;
  setProperty(id: string, key: string, value: string | null): boolean;
  replaceChildren(parentId: string, children: readonly string[]): boolean;
}

const pageMutationPlanSeal = Symbol("page-mutation-plan");

export interface PageMutationPlan<T> {
  readonly [pageMutationPlanSeal]: true;
  readonly pageName: string;
  readonly tag: string;
  readonly value: T;
  readonly candidate: PageDto;
  readonly effects: readonly PageMutationEffect[];
}

/** Optional UI ownership attached to a plan. The token itself is immutable and
 * `isCurrent` must cover the exact selection and mounted Sheet surface that
 * issued the command. Store code checks it before native admission, before
 * publication, and once more before post-commit UI effects. */
export interface PageMutationAuthority<T> {
  readonly token: Readonly<Record<string, unknown>>;
  isCurrent(value: T): boolean;
}

interface InternalPageMutationPlan<T> extends PageMutationPlan<T> {
  graphRoot: string;
  graphEpoch: number;
  graphBinding: number;
  pageGeneration: number;
  editGeneration: number;
  editorTransactionGeneration: number;
  saveBaseline: string | null;
  bindingGeneration: number;
  admitted: boolean;
  captured: Readonly<Record<string, PageMutationDraftNode>>;
  capturedPage: PageMutationDraftPage;
  uiAuthority?: PageMutationAuthority<T>;
}

export type PageMutationDispatch<T> =
  | { kind: "applied"; value: T }
  | { kind: "refused"; claimed: boolean };

const startedPageMutationPlans = new WeakSet<object>();
const appliedPageMutationPlans = new WeakSet<object>();
let pageMutationEffectFailureForTest = false;

function immutableClone<T>(value: T): T {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) {
    return Object.freeze(value.map((item) => immutableClone(item))) as T;
  }
  const clone: Record<string, unknown> = {};
  for (const [key, nested] of Object.entries(value as Record<string, unknown>)) {
    clone[key] = immutableClone(nested);
  }
  return Object.freeze(clone) as T;
}

function immutableNode(node: Node): PageMutationDraftNode {
  return immutableClone(cloneNode(node)) as PageMutationDraftNode;
}

function immutablePage(page: FeedPage): PageMutationDraftPage {
  return immutableClone(clonePages([page])[0]) as PageMutationDraftPage;
}

function clonePageTree(page: FeedPage): Record<string, Node> | null {
  const nodes: Record<string, Node> = {};
  const visit = (id: string): boolean => {
    if (nodes[id]) return true;
    const current = doc.byId[id];
    if (!current || current.page !== page.name) return false;
    nodes[id] = cloneNode(unwrap(current));
    return current.children.every(visit);
  };
  return page.roots.every(visit) ? nodes : null;
}

/** Build a pure detached draft. The callback sees only the captured page tree;
 * it cannot publish, dirty, save, select, or enter an editor. */
export function createPageMutationPlan<T>(
  pageName: string,
  tag: string,
  build: (draft: PageMutationDraft) => T | null,
  uiAuthority?: PageMutationAuthority<T>,
): PageMutationPlan<T> | null {
  const livePage = pageByName(pageName);
  if (!livePage || !pageWritable(pageName) || graphTransitioning()) return null;
  const generation = pageInstanceGeneration(pageName);
  if (generation === null) return null;
  const draftPage = clonePages([unwrap(livePage)])[0];
  const draftNodes = clonePageTree(livePage);
  if (!draftNodes) return null;
  const capturedMutable: Record<string, Node> = {};
  for (const [id, node] of Object.entries(draftNodes)) capturedMutable[id] = cloneNode(node);
  const effects: PageMutationEffect[] = [];
  let active = true;

  const remove = (id: string): boolean => {
    const node = draftNodes[id];
    if (!node) return false;
    const siblings = node.parent === null
      ? draftPage.roots
      : draftNodes[node.parent]?.children;
    if (!siblings) return false;
    const at = siblings.indexOf(id);
    if (at < 0) return false;
    siblings.splice(at, 1);
    const descend = (childId: string) => {
      const child = draftNodes[childId];
      if (!child) return;
      for (const grandchild of [...child.children]) descend(grandchild);
      delete draftNodes[childId];
    };
    descend(id);
    effects.push({ kind: "delete", id });
    return true;
  };

  const draft: PageMutationDraft = {
    page: immutablePage(draftPage),
    node: (id) => active && draftNodes[id] ? immutableNode(draftNodes[id]) : undefined,
    createChild(parentId, at, raw = "") {
      if (!active) return null;
      const parent = draftNodes[parentId];
      if (!parent || at < 0 || at > parent.children.length) return null;
      const id = freshId();
      const node: Node = {
        id,
        raw,
        collapsed: false,
        parent: parentId,
        page: pageName,
        children: [],
      };
      draftNodes[id] = node;
      parent.children.splice(at, 0, id);
      effects.push({ kind: "create", node: immutableNode(node), parent: parentId, at });
      return id;
    },
    insertOutlineChildren(parentId, outlines) {
      if (!active) return null;
      const parent = draftNodes[parentId];
      if (!parent || !outlines.length) return null;
      const format = draftPage.format;
      let last: string | null = null;
      const create = (outline: OutlineNode, parent: string): string => {
        const id = freshId();
        const raw = rawWithInheritedOrderListType(outline.raw, format, parentId);
        draftNodes[id] = { id, raw, collapsed: false, parent, page: pageName, children: [] };
        const at = draftNodes[parent]?.children.length ?? 0;
        draftNodes[parent]?.children.push(id);
        effects.push({ kind: "create", node: immutableNode(draftNodes[id]), parent, at });
        const children = outline.children.map((child) => create(child, id));
        draftNodes[id].children = children;
        return id;
      };
      const created = outlines.map((outline) => create(outline, parentId));
      last = created[created.length - 1] ?? null;
      return last;
    },
    deleteSubtree(id) {
      return active && remove(id);
    },
    setRaw(id, raw) {
      if (!active) return false;
      const node = draftNodes[id];
      if (!node) return false;
      node.raw = raw;
      effects.push({ kind: "raw", id, raw });
      return true;
    },
    setProperty(id, key, value) {
      if (!active) return false;
      const node = draftNodes[id];
      if (!node) return false;
      node.raw = draftPage.format === "org"
        ? orgRawWithProperty(node.raw, key, value)
        : markdownRawWithProperty(node.raw, key, value);
      effects.push({ kind: "property", id, key, value, raw: node.raw });
      return true;
    },
    replaceChildren(parentId, children) {
      if (!active) return false;
      const parent = draftNodes[parentId];
      if (!parent || children.some((id) => !draftNodes[id] || draftNodes[id].page !== pageName)) return false;
      parent.children = [...children];
      for (const id of children) {
        if (draftNodes[id].parent !== parentId) {
          draftNodes[id].parent = parentId;
          effects.push({ kind: "parent", id, parent: parentId });
        }
      }
      effects.push({ kind: "order", parent: parentId, children: [...children] });
      return true;
    },
  };

  let builtValue: T | null;
  try {
    builtValue = build(draft);
  } finally {
    active = false;
  }
  if (builtValue === null) return null;
  const frozenEffects = immutableClone(effects) as readonly PageMutationEffect[];
  const capturedPage = immutablePage(draftPage);
  const captured = immutableClone(Object.fromEntries(
    Object.entries(capturedMutable).map(([id, node]) => [id, immutableNode(node)]),
  )) as Readonly<Record<string, PageMutationDraftNode>>;
  const replay = replayPageMutationEffects(capturedPage, captured, frozenEffects);
  if (!replay) return null;
  const candidate = projectPageDto(replay.page, replay.nodes, false);
  if (!candidate) return null;
  const value = immutableClone(builtValue);
  const admission = graphBindingRuntime.snapshot().applicationPageAdmission;
  const graphRoot = graphMeta()?.root ?? "";
  const epoch = graphEpoch();
  const binding = graphBinding();
  const plan: InternalPageMutationPlan<T> = {
    [pageMutationPlanSeal]: true,
    pageName,
    tag,
    value,
    candidate: immutableClone(candidate),
    effects: frozenEffects,
    graphRoot,
    graphEpoch: epoch,
    graphBinding: binding,
    pageGeneration: generation,
    editGeneration: editGeneration(pageName),
    editorTransactionGeneration: editorTransactionGeneration(pageName),
    saveBaseline: saveBaselineFor(pageName),
    bindingGeneration: admission?.binding_generation ?? -1,
    admitted: admission != null,
    captured,
    capturedPage,
    uiAuthority: uiAuthority
      ? Object.freeze({ token: immutableClone(uiAuthority.token), isCurrent: uiAuthority.isCurrent })
      : undefined,
  };
  return Object.freeze(plan);
}

interface ReplayedPageMutation {
  page: FeedPage;
  nodes: Record<string, Node>;
}

function mutablePage(page: PageMutationDraftPage | FeedPage): FeedPage {
  return clonePages([page as FeedPage])[0];
}

function mutableNodes(nodes: Readonly<Record<string, PageMutationDraftNode>>): Record<string, Node> {
  return Object.fromEntries(Object.entries(nodes).map(([id, node]) => [id, cloneNode(node as Node)]));
}

function applyEffectsToMutablePage(
  page: FeedPage,
  nodes: Record<string, Node>,
  effects: readonly PageMutationEffect[],
): boolean {
  if (pageMutationEffectFailureForTest) return false;
  for (const effect of effects) {
    if (effect.kind === "create") {
      const parent = nodes[effect.parent];
      if (nodes[effect.node.id] || !parent || effect.at < 0 || effect.at > parent.children.length) return false;
      const node = cloneNode(effect.node as Node);
      if (node.page !== page.name || node.parent !== effect.parent || node.children.length) return false;
      nodes[node.id] = node;
      parent.children.splice(effect.at, 0, node.id);
      continue;
    }
    if (effect.kind === "delete") {
      const node = nodes[effect.id];
      if (!node) return false;
      const siblings = node.parent === null ? page.roots : nodes[node.parent]?.children;
      if (!siblings) return false;
      const at = siblings.indexOf(effect.id);
      if (at < 0) return false;
      siblings.splice(at, 1);
      const remove = (id: string): boolean => {
        const current = nodes[id];
        if (!current) return false;
        for (const child of [...current.children]) if (!remove(child)) return false;
        delete nodes[id];
        return true;
      };
      if (!remove(effect.id)) return false;
      continue;
    }
    if (effect.kind === "raw") {
      const node = nodes[effect.id];
      if (!node) return false;
      node.raw = effect.raw;
      continue;
    }
    if (effect.kind === "property") {
      const node = nodes[effect.id];
      if (!node) return false;
      const raw = page.format === "org"
        ? orgRawWithProperty(node.raw, effect.key, effect.value)
        : markdownRawWithProperty(node.raw, effect.key, effect.value);
      if (raw !== effect.raw) return false;
      node.raw = raw;
      continue;
    }
    if (effect.kind === "parent") {
      const node = nodes[effect.id];
      if (!node || !nodes[effect.parent]) return false;
      node.parent = effect.parent;
      continue;
    }
    const parent = nodes[effect.parent];
    if (!parent || new Set(effect.children).size !== effect.children.length) return false;
    if (effect.children.some((id) => !nodes[id] || nodes[id].page !== page.name)) return false;
    parent.children = [...effect.children];
  }

  const seen = new Set<string>();
  const visit = (id: string, parent: string | null): boolean => {
    const node = nodes[id];
    if (!node || seen.has(id) || node.page !== page.name || node.parent !== parent) return false;
    seen.add(id);
    return node.children.every((child) => visit(child, id));
  };
  if (new Set(page.roots).size !== page.roots.length || !page.roots.every((id) => visit(id, null))) return false;
  return Object.values(nodes).every((node) => node.page !== page.name || seen.has(node.id));
}

function replayPageMutationEffects(
  page: PageMutationDraftPage,
  captured: Readonly<Record<string, PageMutationDraftNode>>,
  effects: readonly PageMutationEffect[],
): ReplayedPageMutation | null {
  const replay = { page: mutablePage(page), nodes: mutableNodes(captured) };
  return applyEffectsToMutablePage(replay.page, replay.nodes, effects) ? replay : null;
}

function replayMatchesCandidate(plan: InternalPageMutationPlan<unknown>): boolean {
  const replay = replayPageMutationEffects(plan.capturedPage, plan.captured, plan.effects);
  const projected = replay && projectPageDto(replay.page, replay.nodes, false);
  return !!projected && JSON.stringify(projected) === JSON.stringify(plan.candidate);
}

/** Test-only proof that the finalized authority is a deeply immutable replay
 * program. Returns false in production builds. */
export function __pageMutationPlanDeeplyFrozenForTest(plan: PageMutationPlan<unknown>): boolean {
  if (import.meta.env.MODE !== "test") return false;
  const check = (value: unknown): boolean => {
    if (value === null || typeof value !== "object") return true;
    if (!Object.isFrozen(value)) return false;
    return Object.values(value).every(check);
  };
  return check(plan.effects) && check(plan.candidate) && check(plan.value);
}

function pageMutationPlanCurrent(
  plan: InternalPageMutationPlan<unknown>,
  checkUiAuthority = true,
): boolean {
  const admission = graphBindingRuntime.snapshot().applicationPageAdmission;
  if (
    graphTransitioning()
    || (graphMeta()?.root ?? "") !== plan.graphRoot
    || graphEpoch() !== plan.graphEpoch
    || graphBinding() !== plan.graphBinding
    || pageInstanceGeneration(plan.pageName) !== plan.pageGeneration
    || editGeneration(plan.pageName) !== plan.editGeneration
    || editorTransactionGeneration(plan.pageName) !== plan.editorTransactionGeneration
    || saveBaselineFor(plan.pageName) !== plan.saveBaseline
    || admission?.binding_generation !== plan.bindingGeneration
  ) return false;
  const page = pageByName(plan.pageName);
  if (!page || JSON.stringify(mutablePage(page)) !== JSON.stringify(mutablePage(plan.capturedPage))) return false;
  const liveIds = Object.values(doc.byId).filter((node) => node.page === plan.pageName).map((node) => node.id).sort();
  const capturedIds = Object.keys(plan.captured).sort();
  if (liveIds.length !== capturedIds.length || liveIds.some((id, index) => id !== capturedIds[index])) return false;
  for (const [id, expected] of Object.entries(plan.captured)) {
    const current = doc.byId[id];
    if (!current
      || current.page !== expected.page
      || current.parent !== expected.parent
      || current.raw !== expected.raw
      || current.collapsed !== expected.collapsed
      || current.children.length !== expected.children.length
      || current.children.some((child, index) => child !== expected.children[index])) return false;
  }
  return !checkUiAuthority || !plan.uiAuthority || plan.uiAuthority.isCurrent(plan.value);
}

function applyPageMutationPlanNow<T>(
  plan: InternalPageMutationPlan<T>,
  checkUiAuthority: boolean,
): boolean {
  if (appliedPageMutationPlans.has(plan)
    || !pageMutationPlanCurrent(plan, checkUiAuthority)
    || !replayMatchesCandidate(plan)) return false;
  const pageIndex = doc.pages.findIndex((page) => page.name === plan.pageName);
  if (pageIndex < 0) return false;
  const livePage = mutablePage(doc.pages[pageIndex]);
  const liveNodes = Object.fromEntries(
    Object.values(doc.byId)
      .filter((node) => node.page === plan.pageName)
      .map((node) => [node.id, cloneNode(unwrap(node))]),
  );
  if (!applyEffectsToMutablePage(livePage, liveNodes, plan.effects)) return false;
  const projected = projectPageDto(livePage, liveNodes, false);
  if (!projected || JSON.stringify(projected) !== JSON.stringify(plan.candidate)) return false;
  pushUndo(plan.tag, [plan.pageName]);
  setDoc(produce((state) => {
    if (!applyEffectsToMutablePage(state.pages[pageIndex], state.byId, plan.effects)) {
      throw new Error("validated page-mutation effects failed during atomic replay");
    }
  }));
  appliedPageMutationPlans.add(plan);
  markDirty(plan.pageName);
  return true;
}

const pageMutationRefusedToast = "This Sheet change could not be applied. Nothing was changed.";

/** Apply a plan synchronously. Direct Files is the only authority; a plan whose
 * page has no writable authority is refused without touching the store. */
export function applyPageMutationPlan<T>(
  publicPlan: PageMutationPlan<T>,
  afterApply?: (value: T) => void,
): PageMutationDispatch<T> {
  const plan = publicPlan as InternalPageMutationPlan<T>;
  if (!plan[pageMutationPlanSeal] || startedPageMutationPlans.has(plan)) {
    return { kind: "refused", claimed: !plan.admitted };
  }
  startedPageMutationPlans.add(plan);
  if (!plan.admitted) {
    pushToast(pageMutationRefusedToast, "error");
    return { kind: "refused", claimed: true };
  }
  if (!applyPageMutationPlanNow(plan, false)) return { kind: "refused", claimed: false };
  afterApply?.(plan.value);
  return { kind: "applied", value: plan.value };
}
export { projectPageDto, toDto };
