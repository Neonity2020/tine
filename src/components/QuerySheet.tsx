import {
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  createUniqueId,
  onCleanup,
  onMount,
  type Accessor,
  type JSX,
} from "solid-js";
import { backend } from "../backend";
import {
  ADVANCED_PHRASE,
  MAX_QUERY_BUILDER_DEPTH,
  addChild,
  betweenFilter,
  builderLeafKind,
  contentFilter,
  encodePropertyLeaf,
  filterChildren,
  filterLabel,
  filterValueLabel,
  filterPhrase,
  groupWithPrevious,
  journalFilter,
  namespaceFilter,
  onPageFilter,
  pageRefFilter,
  pageTagsFilter,
  planningFilter,
  priorityFilter,
  propertyFilter,
  propertyLeafTest,
  propertyOperatorArity,
  propertyOperatorLabel,
  propertyOperators,
  querySentence,
  removeAt,
  replaceAt,
  searchFilter,
  setOp,
  taskFilter,
  unwrapAt,
  wrapAt,
  BETWEEN_FIELDS,
  MARKERS,
  PRIORITIES,
  type BetweenField,
  type BuilderLeafKind,
  type PhraseSegment,
  type PropertyLeafTest,
  type PropertyOperatorId,
} from "../editor/queryBuilder";
import type {
  Anchor,
  Cardinality,
  Diagnostic,
  Filter,
  ObservedType,
  Query,
  RegistryRow,
} from "../editor/queryIr";
import { PropertyType, effectiveTypeOf, registryRowFor } from "./PropertyType";
import { Listbox, stop, type ListboxOption } from "./QueryListbox";
import { QueryVocabularyPicker, type VocabularyChoice } from "./QueryVocabularyPicker";
import { DATE_PRESETS, previewDate } from "../editor/dateExpr";
import { dismissOnOutsidePointer, registerTransientLayer, type TransientLayer } from "../transientLayers";

// **The query builder's two states (SPEC §7.2, design §2.1).**
//
// At rest a query block is one plain-English SENTENCE — typography, not a
// control panel. Editing expands a SHEET below it: the anchor line, one row per
// condition, and the view controls in its footer. The chip bar this replaced
// was a permanently visible widget per part of every leaf, which is the shape
// every product in the design survey with a nine-controls-per-row filter panel
// has, and it is the shape a phone cannot draw.
//
// Three properties this file is responsible for:
//
//  - **Bounded rendering (I-22).** Everything here — rows, group nesting,
//    sentence segments, the `⟨advanced⟩` chip's own text — stops at
//    `MAX_QUERY_BUILDER_DEPTH`. A 64-deep query still parses, still runs and
//    still round-trips; it just draws as a short sentence and a short sheet.
//  - **No graph-level work on a render path (I-13).** The registry is read once
//    when the sheet opens, never per row and never per keystroke. The facets
//    resource is the host's, and it is keyed `undefined` until a sheet opens.
//  - **One dismissal ladder (GH #472).** Every menu in here registers with
//    `registerVisiblePopover` under the sheet's own layer id, so Escape, Android
//    Back and an outside press close the innermost thing first.

// `stop` and the listbox keyboard/ARIA controller live in `QueryListbox.tsx`
// now: P4's vocabulary picker needs the SAME controller over a virtualized list
// body, and a second arrow-key implementation for it is the twin D-14 forbids
// (N1). They are re-exported here because this module is where the sheet's
// callers already import them from.
export { stop, Listbox, type ListboxOption };

/** **The property registry, read once per opened sheet (§6.4, I-13, N4).**
 *
 *  The registry is a graph-level table. Asking for it on a keystroke, or once
 *  per rendered row, is the shape this campaign exists to delete — so the HOST
 *  holds one shared read and hands it down.
 *
 *  Two things moved in P4. The read is now the sheet's ONLY graph-level
 *  question (`query_facets(false)` is gone with the two-stage property chooser
 *  it fed), and acquiring it is the HOST's job: opening a sheet makes the
 *  resource key live, which is not the same event as a declaration landing. So
 *  the sheet no longer calls `request()` on mount — `request()` means "a
 *  declaration was written, re-read", and nothing else. */
export interface RegistryAccess {
  rows: Accessor<RegistryRow[] | undefined>;
  /** The read for the current graph/declaration revision is in flight. Every
   *  edit whose meaning depends on a key's effective type waits for it rather
   *  than falling back to the untyped `text` family (§6.3). */
  pending: Accessor<boolean>;
  request: () => void;
}

const locKey = (l: number[]) => l.join(".");

/** `openMenu`'s key for the add-condition chooser. Rows key by their location;
 *  there is only ever one chooser, at the root. */
const ADD_MENU_KEY = "add";

/** Every popover in the sheet — anchor menu, field chooser, operator menu, value
 *  editors, and the sort/summarize pickers in the footer — registers here, so
 *  all of them answer Escape/Back AND "the user pressed somewhere else" the same
 *  way. GH #472 is what happens when they do not: two of the four had
 *  hand-rolled the outside-press effect and two had not, so a menu stayed open
 *  while the user clicked into and edited a different block. The trigger is
 *  passed as inside-the-popover so its own click can toggle. */
export function registerVisiblePopover(open: () => boolean, layer: TransientLayer) {
  createEffect(() => {
    if (!open()) return;
    const unregister = registerTransientLayer(layer);
    onCleanup(unregister);
  });
  dismissOnOutsidePointer({
    open,
    inside: () => [layer.root?.(), layer.trigger?.()],
    dismiss: () => layer.dismiss("explicit"),
  });
}

// ---------------------------------------------------------------------------
// The resting sentence (§7.2)
// ---------------------------------------------------------------------------

/** A single condition rather than a group — the unit that renders as ONE row,
 *  and the unit a `not`/`off` wrapper decorates without costing a level. */
function isLeafLike(filter: Filter): boolean {
  return (
    filter.kind === "leaf" ||
    filter.kind === "raw" ||
    filter.kind === "true" ||
    filter.kind === "false"
  );
}

/** Where a retained leaf sits, and whether an `Off` encloses it. */
export interface RawLeafSite {
  leaf: Filter & { kind: "raw" };
  loc: number[];
  /** Derived from the CURRENT tree, never stored on the node (§4.3.2): a
   *  diagnostic inside an `Off` subtree does not invalidate the query. */
  disabled: boolean;
}

/** The `raw` leaves of a tree, in order — the red rows, and the conditions the
 *  anchor prompt names. Walks the tree rather than matching spans: spans are
 *  presentation metadata (§4.3.2) and are absent whenever the pre-pass rewrote
 *  anything, so a span is never the way to find a leaf. */
export function rawLeaves(filter: Filter, loc: number[] = [], disabled = false): RawLeafSite[] {
  if (filter.kind === "raw") return [{ leaf: filter, loc, disabled }];
  const children = filterChildren(filter);
  if (!children) return [];
  const off = disabled || filter.kind === "off";
  return children.flatMap((child, index) => rawLeaves(child, [...loc, index], off));
}

/** How many conditions a tree holds — what the anchor prompt counts against.
 *  Bounded by the tree it is given, and it never recurses into a relation
 *  predicate, because a predicate is part of ONE condition. */
export function countConditions(filter: Filter): number {
  if (isLeafLike(filter)) return 1;
  const children = filterChildren(filter);
  if (!children) return 1;
  return children.reduce((total, child) => total + countConditions(child), 0);
}

/** The diagnostic that explains a retained leaf, matched by kind. `Raw` nodes
 *  and their diagnostics travel together (§4.3.2) and there is at most one
 *  diagnostic per retained kind in practice; when there are several the first
 *  of that kind is the honest thing to show, and it is never invented. */
function diagnosticFor(query: Query | undefined, leaf: Filter & { kind: "raw" }): Diagnostic | undefined {
  return (query?.diagnostics ?? []).find((d) => d.kind === leaf.diagnostic_kind);
}

/**
 * **The resting state: one sentence, the count, and a ⚙ (§7.2, design §2.1).**
 *
 * Nothing here is a control except the ⚙ and the sentence itself. The sentence
 * IS the affordance — clicking it, or pressing Enter or Space on it, opens the
 * sheet — which is why it carries a button role and a visible focus ring rather
 * than looking like text that happens to be clickable.
 */
export function QuerySentence(props: {
  query: Query;
  total?: JSX.Element;
  onOpen: () => void;
  open?: boolean;
  sentenceRef?: (element: HTMLSpanElement) => void;
}): JSX.Element {
  const segments = createMemo<PhraseSegment[]>(() =>
    querySentence({ anchor: props.query.anchor, filter: props.query.filter }),
  );
  // A retained leaf reads as its decoded text; the diagnostic is why it is red,
  // so it is the hover text rather than more words in the line.
  const rawTitles = createMemo(() => {
    const titles = new Map<string, string>();
    for (const site of rawLeaves(props.query.filter)) {
      const diagnostic = diagnosticFor(props.query, site.leaf);
      if (diagnostic) titles.set(site.leaf.text, diagnostic.message);
    }
    return titles;
  });
  return (
    <div class="qs-line" onClick={stop}>
      <span
        ref={props.sentenceRef}
        class="qs-sentence"
        role="button"
        tabindex="0"
        aria-expanded={props.open ? "true" : "false"}
        title="Click to edit this query"
        onClick={(e) => {
          stop(e);
          props.onOpen();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            e.stopPropagation();
            props.onOpen();
          }
        }}
      >
        <For each={segments()}>
          {(segment) => (
            <span
              class="qs-seg"
              classList={{
                "qs-seg-value": segment.kind === "value",
                "qs-seg-field": segment.kind === "field",
                "qs-seg-advanced": segment.kind === "advanced",
              }}
              title={segment.title ?? rawTitles().get(segment.text)}
            >
              {segment.text}
            </span>
          )}
        </For>
      </span>
      <Show when={props.total != null}>
        <span class="qs-count-slot">{props.total}</span>
      </Show>
      <button
        type="button"
        class="qs-gear"
        aria-label="Edit query filter"
        aria-expanded={props.open ? "true" : "false"}
        title="Edit query filter"
        onClick={(e) => {
          stop(e);
          props.onOpen();
        }}
      >
        ⚙
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Row and group model — the tree, as the sheet draws it
// ---------------------------------------------------------------------------

/** The 13 field types a row can be (§7.4's P3 vocabulary).
 *
 *  The 14th `BuilderLeafKind`, `search`, is deliberately NOT offered as a new
 *  field — it never was — but an existing `search` leaf still renders as a row
 *  and still edits, which is the difference between a cap and a refusal. P4
 *  replaces the CONTENTS of this chooser with the registry-backed vocabulary
 *  picker; the keyboard and aria skeleton below is what it swaps a data source
 *  into. */
export const FILTER_TYPES: { kind: BuilderLeafKind; label: string }[] = [
  { kind: "page", label: "Page / tag reference" },
  { kind: "task", label: "Task marker" },
  { kind: "priority", label: "Priority" },
  { kind: "property", label: "Property" },
  { kind: "scheduled", label: "Scheduled" },
  { kind: "deadline", label: "Deadline" },
  { kind: "journal", label: "On journal page" },
  { kind: "between", label: "Between dates" },
  { kind: "content", label: "Full-text search" },
  { kind: "onPage", label: "On page" },
  { kind: "namespace", label: "In namespace" },
  { kind: "pageProperty", label: "Page property" },
  { kind: "pageTags", label: "Page tags" },
];

const FIELD_LABELS: Record<BuilderLeafKind, string> = {
  page: "Page / tag reference",
  task: "Task marker",
  priority: "Priority",
  property: "Property",
  scheduled: "Scheduled",
  deadline: "Deadline",
  journal: "On journal page",
  between: "Between dates",
  content: "Full-text search",
  onPage: "On page",
  namespace: "In namespace",
  pageProperty: "Page property",
  pageTags: "Page tags",
  search: "Full-text search",
};

/** The fixed operator phrase for a non-property row, and its negation.
 *  §7.4: *is not* / *does not reference* / *is none of* emit `not(<positive
 *  leaf>)`, and a `not` whose inner is a single leaf renders as that row with
 *  the negative operator selected — never as a separate NOT wrapper row. */
const KIND_PHRASE: Record<BuilderLeafKind, { positive: string; negative: string }> = {
  page: { positive: "references", negative: "does not reference" },
  task: { positive: "is any of", negative: "is none of" },
  priority: { positive: "is any of", negative: "is none of" },
  property: { positive: "is", negative: "is not" },
  scheduled: { positive: "is set", negative: "is not set" },
  deadline: { positive: "is set", negative: "is not set" },
  journal: { positive: "is a journal page", negative: "is not a journal page" },
  between: { positive: "between", negative: "not between" },
  content: { positive: "contains", negative: "does not contain" },
  onPage: { positive: "on page", negative: "not on page" },
  namespace: { positive: "in namespace", negative: "not in namespace" },
  pageProperty: { positive: "is", negative: "is not" },
  pageTags: { positive: "is any of", negative: "is none of" },
  search: { positive: "matches", negative: "does not match" },
};

/** One thing the sheet draws. A group holds rows; a row holds one condition;
 *  `advanced` is the bounded stand-in for a subtree past the rendering cap. */
type SheetNode =
  | {
      kind: "row";
      /** The OUTERMOST node — the `off`/`not` wrapper when there is one. */
      loc: number[];
      filter: Filter;
      /** The condition inside the wrappers. */
      core: Filter;
      negated: boolean;
      disabled: boolean;
    }
  | {
      kind: "group";
      loc: number[];
      /** Where the `and`/`or` itself lives, which is inside a `not` for the
       *  `none of` / `not all of` headers. */
      opLoc: number[];
      header: "all of" | "any of" | "none of" | "not all of";
      negated: boolean;
      disabled: boolean;
      children: SheetNode[];
    }
  | { kind: "advanced"; loc: number[]; filter: Filter };

/**
 * **The tree, as rows and groups (§7.4, design §2.5).**
 *
 * The stored form is rendered HONESTLY: §3.5 forbids De Morgan rewriting, so a
 * `not` over an `or` reads "none of" and a `not` over an `and` reads "not all
 * of" — the builder never silently restates the user's query as its dual.
 *
 * A `not`/`off` around a SINGLE condition is not a level: it is the row's
 * negative operator and the row's greyed state. Around a group it is the
 * group's header and the group's greyed state.
 */
function buildNodes(filter: Filter, loc: number[], depth: number): SheetNode {
  if (depth >= MAX_QUERY_BUILDER_DEPTH) return { kind: "advanced", loc, filter };
  let node = filter;
  let at = loc;
  let negated = false;
  let disabled = false;
  // Peel the decorations. `off` is P6's to toggle, but §3.5 requires the state
  // to RENDER now: a disabled row is present, round-trips, and does not run.
  for (;;) {
    if (node.kind === "off" && !disabled) {
      disabled = true;
      node = node.inner;
      at = [...at, 0];
      continue;
    }
    if (node.kind === "not" && !negated && (isLeafLike(node.inner) || node.inner.kind === "off")) {
      negated = true;
      node = node.inner;
      at = [...at, 0];
      continue;
    }
    break;
  }
  if (node.kind === "and" || node.kind === "or") {
    return {
      kind: "group",
      loc,
      opLoc: at,
      header: negated ? (node.kind === "or" ? "none of" : "not all of") : node.kind === "or" ? "any of" : "all of",
      negated,
      disabled,
      children: node.items.map((item, index) => buildNodes(item, [...at, index], depth + 1)),
    };
  }
  if (node.kind === "not") {
    // A `not` over a group: the group's own header carries it.
    const inner = node.inner;
    if (inner.kind === "and" || inner.kind === "or") {
      return {
        kind: "group",
        loc,
        opLoc: [...at, 0],
        header: inner.kind === "or" ? "none of" : "not all of",
        negated: true,
        disabled,
        children: inner.items.map((item, index) => buildNodes(item, [...at, 0, index], depth + 1)),
      };
    }
    return { kind: "row", loc, filter, core: inner, negated: true, disabled };
  }
  return { kind: "row", loc, filter, core: node, negated, disabled };
}

/** A popover anchored to a trigger button, registered in the dismissal ladder. */
function Popover(props: {
  open: () => boolean;
  close: () => void;
  parentId?: string;
  trigger: () => HTMLElement | null;
  children: (rootRef: (element: HTMLDivElement) => void) => JSX.Element;
}): JSX.Element {
  let rootEl: HTMLDivElement | undefined;
  const layerId = `query-sheet-menu-${createUniqueId()}`;
  registerVisiblePopover(props.open, {
    id: layerId,
    parentId: props.parentId,
    root: () => rootEl ?? null,
    trigger: props.trigger,
    dismiss: () => {
      props.close();
      return true;
    },
  });
  return (
    <Show when={props.open()}>
      {props.children((element) => {
        rootEl = element;
      })}
    </Show>
  );
}

// ---------------------------------------------------------------------------
// Value editors — lifted intact from the chip bar into the row's value cell
// ---------------------------------------------------------------------------

/** Plain free-text input that commits on Enter. */
function TextInput(props: {
  placeholder: string;
  initial?: string;
  onCommit: (text: string) => void;
}): JSX.Element {
  const [v, setV] = createSignal(props.initial ?? "");
  return (
    <div class="qs-value-editor">
      <input
        class="qs-input"
        autofocus
        placeholder={props.placeholder}
        value={v()}
        onInput={(e) => setV(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && v().trim()) props.onCommit(v().trim());
        }}
      />
    </div>
  );
}

/** Page-name input with fuzzy autocomplete from the graph. Debounced, because
 *  `quick_switch` lists pages from disk and holding a key must not fire an IPC
 *  round trip per character (I-13). */
function PageInput(props: {
  placeholder: string;
  initial?: string;
  onCommit: (name: string) => void;
}): JSX.Element {
  const [q, setQ] = createSignal(props.initial ?? "");
  const [dq, setDq] = createSignal(props.initial ?? "");
  let dqTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const s = q();
    clearTimeout(dqTimer);
    dqTimer = setTimeout(() => setDq(s), 120);
  });
  onCleanup(() => clearTimeout(dqTimer));
  const [matches] = createResource(dq, (s) => backend().quickSwitch(s, 8));
  return (
    <div class="qs-value-editor">
      <input
        class="qs-input"
        autofocus
        placeholder={props.placeholder}
        value={q()}
        onInput={(e) => setQ(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && q().trim()) props.onCommit(q().trim());
        }}
      />
      <For each={matches() ?? []}>
        {(p) => (
          <button type="button" class="qs-option" onClick={() => props.onCommit(p.name)}>
            {p.name}
          </button>
        )}
      </For>
    </div>
  );
}

/** Multi-select (task markers, priorities) with checkboxes + an Add button. */
function MultiPick(props: {
  options: string[];
  initial?: string[];
  onCommit: (picked: string[]) => void;
}): JSX.Element {
  const [picked, setPicked] = createSignal<string[]>(props.initial ?? []);
  const toggle = (o: string) =>
    setPicked(picked().includes(o) ? picked().filter((x) => x !== o) : [...picked(), o]);
  return (
    <div class="qs-value-editor">
      <For each={props.options}>
        {(o) => (
          <label class="qs-check">
            <input type="checkbox" checked={picked().includes(o)} onChange={() => toggle(o)} /> {o}
          </label>
        )}
      </For>
      <button
        type="button"
        class="qs-commit"
        disabled={picked().length === 0}
        onClick={() => props.onCommit(picked())}
      >
        Apply
      </button>
    </div>
  );
}

/** A single date-bound input with a live resolved-date preview underneath. */
function DateBoundInput(props: {
  placeholder: string;
  value: string;
  onInput: (v: string) => void;
  onEnter: () => void;
  autofocus?: boolean;
}): JSX.Element {
  const preview = createMemo(() => previewDate(props.value));
  return (
    <div class="qs-bound">
      <input
        class="qs-input"
        autofocus={props.autofocus}
        placeholder={props.placeholder}
        value={props.value}
        onInput={(e) => props.onInput(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") props.onEnter();
        }}
      />
      <span class="qs-bound-preview">{preview() ? `→ ${preview()}` : " "}</span>
    </div>
  );
}

const BETWEEN_FIELD_LABEL: Record<BetweenField, string> = {
  journal: "Journal date",
  scheduled: "Scheduled",
  deadline: "Deadline",
  any: "Any date",
};

/** Date-range editor: which date, one-click relative presets, and two bound
 *  inputs that accept keywords (`today`), relative offsets (`-30d`), ISO dates
 *  or a journal-page title — each with a live resolved-date preview. */
function BetweenPick(props: {
  onCommit: (field: BetweenField, start: string, end: string) => void;
}): JSX.Element {
  const [field, setField] = createSignal<BetweenField>("journal");
  const [start, setStart] = createSignal("");
  const [end, setEnd] = createSignal("");
  const ready = () => !!start().trim() && !!end().trim();
  const submit = () => {
    if (ready()) props.onCommit(field(), start().trim(), end().trim());
  };
  return (
    <div class="qs-value-editor qs-between">
      <div class="qs-between-field">
        <For each={BETWEEN_FIELDS}>
          {(f) => (
            <button
              type="button"
              class="qs-conn"
              classList={{ active: field() === f }}
              onClick={() => setField(f)}
            >
              {BETWEEN_FIELD_LABEL[f]}
            </button>
          )}
        </For>
      </div>
      <div class="qs-between-presets">
        <For each={DATE_PRESETS}>
          {(p) => (
            <button
              type="button"
              class="qs-preset"
              title={`${p.start} → ${p.end}`}
              onClick={() => {
                setStart(p.start);
                setEnd(p.end);
              }}
            >
              {p.label}
            </button>
          )}
        </For>
      </div>
      <DateBoundInput
        placeholder="Start — today, -30d, 2026-06-01, or a page"
        value={start()}
        onInput={setStart}
        onEnter={submit}
        autofocus
      />
      <DateBoundInput
        placeholder="End — today, +7d, 2026-06-30, or a page"
        value={end()}
        onInput={setEnd}
        onEnter={submit}
      />
      <button type="button" class="qs-commit" disabled={!ready()} onClick={submit}>
        Apply
      </button>
    </div>
  );
}

/** The value collector for a filter kind, and the IR leaf `og.rs` builds for the
 *  same intent. Shared by the add-a-condition flow and a row's own value cell. */
function ValueEditor(props: {
  kind: BuilderLeafKind;
  onCommit: (filter: Filter) => void;
}): JSX.Element {
  return (
    <>
      <Show when={props.kind === "page"}>
        <PageInput placeholder="Page or tag name" onCommit={(name) => props.onCommit(pageRefFilter(name))} />
      </Show>
      <Show when={props.kind === "task"}>
        <MultiPick options={MARKERS} onCommit={(markers) => props.onCommit(taskFilter(markers))} />
      </Show>
      <Show when={props.kind === "priority"}>
        <MultiPick options={PRIORITIES} onCommit={(levels) => props.onCommit(priorityFilter(levels))} />
      </Show>
      <Show when={props.kind === "between"}>
        <BetweenPick onCommit={(field, start, end) => props.onCommit(betweenFilter(field, start, end))} />
      </Show>
      <Show when={props.kind === "onPage"}>
        <PageInput placeholder="Page name" onCommit={(name) => props.onCommit(onPageFilter(name))} />
      </Show>
      <Show when={props.kind === "namespace"}>
        <PageInput placeholder="Namespace (parent page)" onCommit={(ns) => props.onCommit(namespaceFilter(ns))} />
      </Show>
      <Show when={props.kind === "content"}>
        <TextInput placeholder="Text to search for" onCommit={(text) => props.onCommit(contentFilter(text))} />
      </Show>
      <Show when={props.kind === "search"}>
        <TextInput placeholder="Search words and operators" onCommit={(source) => props.onCommit(searchFilter(source))} />
      </Show>
      <Show when={props.kind === "pageTags"}>
        <TextInput placeholder="Tag (one)" onCommit={(t) => props.onCommit(pageTagsFilter([t]))} />
      </Show>
    </>
  );
}

// ---------------------------------------------------------------------------
// The sheet
// ---------------------------------------------------------------------------

/** The counted prompt an anchor switch raises (§7.4, design §2.3). The sheet
 *  renders it; the host computes it, because re-validation is the ENGINE's
 *  answer and the frontend has no "does this leaf apply" oracle (D-14). */
export interface AnchorPrompt {
  anchor: Anchor;
  /** How many conditions do not apply, and how many there are in total. */
  count: number;
  total: number;
  /** The phrases of the conditions that do not apply. */
  names: string[];
  onRemove: () => void;
  onKeep: () => void;
  onCancel: () => void;
}

const ANCHOR_OPTIONS: { key: Anchor; label: string; hint: string }[] = [
  { key: "block", label: "blocks", hint: "individual bullets, anywhere in the graph" },
  { key: "page", label: "pages", hint: "whole pages" },
];

export interface QuerySheetProps {
  anchor: () => Anchor;
  onAnchor: (anchor: Anchor) => void;
  anchorPrompt: () => AnchorPrompt | null;
  /** The `and`/`or` root the sheet edits. */
  root: () => Filter;
  /** The whole query, for the diagnostics a red row shows. */
  query: () => Query | undefined;
  apply: (next: Filter) => void;
  registry: RegistryAccess;
  /** Open and focus the query text pane — the route the `⟨advanced⟩` control
   *  takes, and the route a row that cannot be edited here always has (§7.4). */
  onEditText?: () => void;
  /** The four most frequent property keys, for the empty state's "Try:" line. */
  suggestions: () => string[];
  /** Which menu inside the sheet is open, by row loc + purpose. The HOST owns
   *  this signal, because "is one of my child menus open?" is what decides
   *  whether an outside press closes the menu or the sheet (§7.2). */
  openMenu: () => string | null;
  setOpenMenu: (key: string | null) => void;
  /** The layer every menu in here parents to. */
  layerId?: string;
  /** Open the field chooser on an empty add row the moment the sheet appears —
   *  the `/query` entry (§7.3). */
  autoOpenChooser?: boolean;
  footer?: JSX.Element;
  sheetRef?: (element: HTMLDivElement) => void;
  stale?: boolean;
}

export function QuerySheet(props: QuerySheetProps): JSX.Element {
  const nodes = createMemo(() => buildNodes(props.root(), [], 0));
  const isEmpty = createMemo(() => {
    const node = nodes();
    return node.kind === "group" && node.children.length === 0;
  });
  // The add-condition chooser is one of the sheet's menus, not a signal of its
  // own: sharing `openMenu` is what makes opening it close a row's menu, and
  // what tells the HOST that a press belongs to the menu's rung of the ladder
  // rather than to the sheet's (GH #472).
  const adding = () => props.openMenu() === ADD_MENU_KEY;
  const setAdding = (open: boolean) => props.setOpenMenu(open ? ADD_MENU_KEY : null);
  createEffect(() => {
    if (props.autoOpenChooser) setAdding(true);
  });

  const addAtRoot = (filter: Filter) => {
    props.apply(addChild(props.root(), [], filter));
    setAdding(false);
  };

  return (
    <div
      ref={props.sheetRef}
      class="qs-sheet"
      classList={{ "qs-sheet-stale": props.stale }}
      role="group"
      aria-label="Query filter"
      onClick={stop}
    >
      <AnchorLine
        anchor={props.anchor}
        onAnchor={props.onAnchor}
        empty={isEmpty()}
        openMenu={props.openMenu}
        setOpenMenu={props.setOpenMenu}
        layerId={props.layerId}
      />
      <Show when={props.anchorPrompt()}>{(prompt) => <AnchorPromptPanel prompt={prompt()} />}</Show>
      <NodeList
        node={nodes()}
        isRoot
        sheet={props}
        adding={adding}
        setAdding={setAdding}
        onAdd={addAtRoot}
      />
      <Show when={isEmpty() && props.suggestions().length > 0}>
        <div class="qs-try">
          <span class="qs-try-label">Try:</span>
          <For each={props.suggestions()}>
            {(key) => (
              <button
                type="button"
                class="qs-try-item"
                onClick={() => addAtRoot(propertyFilter(key, null))}
              >
                {key}
              </button>
            )}
          </For>
        </div>
      </Show>
      {/* Read ONCE. `<Show when={x}>{x}</Show>` reads `x` twice — for the
          condition and for the body — and a JSX prop is a getter, so the footer
          (and the text pane inside it, and the crossing notice inside that) was
          instantiated twice, the first copy detached. A detached copy still runs
          `onMount`, which is how a focus grab lands on nothing. */}
      <Show when={props.footer}>{(footer) => <div class="qs-footer">{footer()}</div>}</Show>
    </div>
  );
}

function AnchorLine(props: {
  anchor: () => Anchor;
  onAnchor: (anchor: Anchor) => void;
  empty: boolean;
  openMenu: () => string | null;
  setOpenMenu: (key: string | null) => void;
  layerId?: string;
}): JSX.Element {
  let triggerEl: HTMLButtonElement | undefined;
  const menuId = `qs-anchor-${createUniqueId()}`;
  const open = () => props.openMenu() === "anchor";
  const label = () => (props.anchor() === "page" ? "pages" : "blocks");
  return (
    <div class="qs-anchor">
      {/* Not a row and not deletable: it is the sentence's subject (§7.4). */}
      <span class="qs-anchor-lead">Find</span>
      <span class="qs-anchor-wrap">
        <button
          ref={triggerEl}
          type="button"
          class="qs-anchor-button"
          aria-haspopup="listbox"
          aria-expanded={open() ? "true" : "false"}
          aria-controls={menuId}
          title="What this query selects"
          onClick={(e) => {
            stop(e);
            props.setOpenMenu(open() ? null : "anchor");
          }}
        >
          {label()} ▾
        </button>
        <Popover
          open={open}
          close={() => props.setOpenMenu(null)}
          parentId={props.layerId}
          trigger={() => triggerEl ?? null}
        >
          {(rootRef) => (
            <Listbox
              id={menuId}
              label="What this query selects"
              rootRef={rootRef}
              options={ANCHOR_OPTIONS.map((option) => ({
                key: option.key,
                label: option.label,
                hint: option.hint,
                active: option.key === props.anchor(),
              }))}
              onPick={(key) => {
                props.setOpenMenu(null);
                props.onAnchor(key as Anchor);
              }}
            />
          )}
        </Popover>
      </span>
      <Show when={props.empty}>
        <span class="qs-anchor-tail">where …</span>
      </Show>
    </div>
  );
}

function AnchorPromptPanel(props: { prompt: AnchorPrompt }): JSX.Element {
  let panelEl: HTMLDivElement | undefined;
  onMount(() => panelEl?.focus());
  const row = () => (props.prompt.anchor === "page" ? "pages" : "blocks");
  return (
    <div
      ref={panelEl}
      class="qs-anchor-prompt"
      role="alertdialog"
      aria-label="Switching what this query selects"
      tabindex="-1"
    >
      <p class="qs-anchor-prompt-text">
        Switching to <strong>{row()}</strong> — {props.prompt.count} of your {props.prompt.total}{" "}
        conditions don't apply to {row()}
        <Show when={props.prompt.names.length > 0}>
          {" "}
          (
          <For each={props.prompt.names}>
            {(name, index) => (
              <>
                <Show when={index() > 0}>, </Show>
                <code>{name}</code>
              </>
            )}
          </For>
          )
        </Show>
        .
      </p>
      <div class="qs-anchor-prompt-actions">
        <button type="button" class="qs-commit" onClick={() => props.prompt.onRemove()}>
          Remove them
        </button>
        <button type="button" class="qs-conn" onClick={() => props.prompt.onKeep()}>
          Keep anyway
        </button>
        <button type="button" class="qs-conn" onClick={() => props.prompt.onCancel()}>
          Cancel
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Rows and groups
// ---------------------------------------------------------------------------

function NodeList(props: {
  node: SheetNode;
  isRoot?: boolean;
  sheet: QuerySheetProps;
  adding?: Accessor<boolean>;
  setAdding?: (open: boolean) => void;
  onAdd?: (filter: Filter) => void;
}): JSX.Element {
  return (
    <Show when={props.node.kind === "group"} fallback={<SheetItem node={props.node} sheet={props.sheet} />}>
      {(() => {
        const group = props.node as Extract<SheetNode, { kind: "group" }>;
        const body = (
          <>
            <div
              class="qs-rows"
              role="list"
              aria-label={props.isRoot ? "Query conditions" : `Conditions, ${group.header}`}
            >
              <For each={group.children}>
                {(child) => (
                  <Show
                    when={child.kind === "group"}
                    fallback={<SheetItem node={child} sheet={props.sheet} />}
                  >
                    <div class="qs-listitem" role="listitem">
                      <NodeList node={child} sheet={props.sheet} />
                    </div>
                  </Show>
                )}
              </For>
            </div>
            <Show when={props.isRoot}>
              <AddCondition
                sheet={props.sheet}
                open={props.adding!}
                setOpen={props.setAdding!}
                onAdd={props.onAdd!}
              />
            </Show>
          </>
        );
        return props.isRoot ? (
          body
        ) : (
          <div class="qs-group" classList={{ "qs-off": group.disabled }}>
            <GroupHeader group={group} sheet={props.sheet} />
            {body}
          </div>
        );
      })()}
    </Show>
  );
}

function GroupHeader(props: {
  group: Extract<SheetNode, { kind: "group" }>;
  sheet: QuerySheetProps;
}): JSX.Element {
  let menuTrigger: HTMLButtonElement | undefined;
  const menuKey = () => `group-menu:${locKey(props.group.loc)}`;
  const open = () => props.sheet.openMenu() === menuKey();
  const menuId = `qs-group-menu-${createUniqueId()}`;
  const root = () => props.sheet.root();
  const cycle = () => {
    // Click cycles all ↔ any. `none of` is rarer and lives in the ⋮ menu, so it
    // is never one mis-click away (design §2.5).
    const next = props.group.header === "all of" || props.group.header === "not all of" ? "or" : "and";
    props.sheet.apply(setOp(root(), props.group.opLoc, next));
  };
  return (
    <div class="qs-group-header">
      <button
        type="button"
        class="qs-group-op"
        title="Switch between all of / any of"
        onClick={(e) => {
          stop(e);
          cycle();
        }}
      >
        {props.group.header}
      </button>
      <span class="qs-menu-wrap">
        <button
          ref={menuTrigger}
          type="button"
          class="qs-row-menu"
          aria-label="Group actions"
          aria-haspopup="listbox"
          aria-expanded={open() ? "true" : "false"}
          aria-controls={menuId}
          onClick={(e) => {
            stop(e);
            props.sheet.setOpenMenu(open() ? null : menuKey());
          }}
        >
          ⋮
        </button>
        <Popover
          open={open}
          close={() => props.sheet.setOpenMenu(null)}
          parentId={props.sheet.layerId}
          trigger={() => menuTrigger ?? null}
        >
          {(rootRef) => (
            <Listbox
              id={menuId}
              label="Group actions"
              rootRef={rootRef}
              options={[
                { key: "none", label: props.group.negated ? "Remove none of" : "None of" },
                { key: "ungroup", label: "Ungroup" },
                { key: "remove", label: "Remove" },
              ]}
              onPick={(key) => {
                props.sheet.setOpenMenu(null);
                if (key === "none") {
                  props.sheet.apply(
                    props.group.negated
                      ? unwrapAt(root(), props.group.loc)
                      : wrapAt(root(), props.group.opLoc, "not"),
                  );
                } else if (key === "ungroup") {
                  props.sheet.apply(unwrapAt(root(), props.group.opLoc));
                } else {
                  props.sheet.apply(removeAt(root(), props.group.loc));
                }
              }}
            />
          )}
        </Popover>
      </span>
    </div>
  );
}

function SheetItem(props: { node: SheetNode; sheet: QuerySheetProps }): JSX.Element {
  return (
    <Show
      when={props.node.kind === "advanced"}
      fallback={<QueryRow node={props.node as Extract<SheetNode, { kind: "row" }>} sheet={props.sheet} />}
    >
      <AdvancedChip node={props.node as Extract<SheetNode, { kind: "advanced" }>} sheet={props.sheet} />
    </Show>
  );
}

/** A subtree past the rendering cap: ONE control, its phrase, and a ×. The
 *  language never refuses it and the pane below shows the whole query, so the
 *  chip is a presentation state — not a `Raw`, not a truncation, not a drop
 *  (§7.4).
 *
 *  **It is a control, not a label (§7.5).** It used to be a `<span>` with a
 *  `title`, which is unreachable by keyboard and invisible to a screen reader:
 *  the one part of the sheet that says "this is edited somewhere else" was the
 *  one part that could not take you there. Pressing it opens and focuses the
 *  query text pane, where the whole subtree is.
 *
 *  It does NOT select a span. A folded subtree has no offset that can be
 *  associated with the engine-printed draft unambiguously — the printer decides
 *  the layout, and this node's `loc` is a position in the tree, not in the text
 *  — so the honest action is to put the cursor in the pane and let the user
 *  read, rather than to highlight a guess. The real subtree is untouched: it is
 *  never replaced by a `Raw` merely so that it can be drawn. */
function AdvancedChip(props: {
  node: Extract<SheetNode, { kind: "advanced" }>;
  sheet: QuerySheetProps;
}): JSX.Element {
  const phrase = () => `${ADVANCED_PHRASE} ${filterLabel(props.node.filter)}`;
  return (
    <div class="qs-row qs-row-advanced" role="listitem">
      <Show
        when={props.sheet.onEditText}
        fallback={<span class="qs-advanced" title="edit in the query text below">{phrase()}</span>}
      >
        <button
          type="button"
          class="qs-advanced qs-advanced-open"
          title="Edit this part in the query text below"
          aria-label={`Edit in the query text: ${filterLabel(props.node.filter)}`}
          onClick={(e) => {
            stop(e);
            props.sheet.onEditText?.();
          }}
        >
          {phrase()}
        </button>
      </Show>
      <button
        type="button"
        class="qs-row-remove"
        aria-label="Remove condition"
        title="Remove"
        onClick={(e) => {
          stop(e);
          props.sheet.apply(removeAt(props.sheet.root(), props.node.loc));
        }}
      >
        ×
      </button>
    </div>
  );
}

/** The effective type of the key a property row tests, from the ONE registry
 *  read. An unknown key gets the text family, which is the untyped
 *  `= '<text>'` the builder could always say. */
function effectiveFor(
  registry: RegistryAccess,
  key: string | undefined,
): { type: ObservedType; cardinality: Cardinality } {
  const row = key ? registryRowFor(registry.rows(), key) : undefined;
  return row ? effectiveTypeOf(row) : { type: "text", cardinality: "one" };
}

function QueryRow(props: {
  node: Extract<SheetNode, { kind: "row" }>;
  sheet: QuerySheetProps;
}): JSX.Element {
  const core = () => props.node.core;
  const root = () => props.sheet.root();
  const raw = () => (core().kind === "raw" ? (core() as Filter & { kind: "raw" }) : null);
  const kind = () => builderLeafKind(core());
  const property = createMemo<PropertyLeafTest | null>(() => {
    const test = propertyLeafTest(props.node.filter);
    if (!test) return null;
    // Re-read with the key's effective type so `references` and `is` — the one
    // pair of identities with the same IR spelling — come back as themselves.
    return propertyLeafTest(props.node.filter, effectiveFor(props.sheet.registry, test.key));
  });
  const effective = createMemo(() => effectiveFor(props.sheet.registry, property()?.key));
  const fieldLabel = () => {
    const test = property();
    if (test) return test.throughPage ? "Page property" : "Property";
    const k = kind();
    return k ? FIELD_LABELS[k] : "Condition";
  };
  const operatorLabel = () => {
    const test = property();
    if (test) return propertyOperatorLabel(test.id, effective().cardinality);
    const k = kind();
    if (!k) return props.node.negated ? "not" : "matches";
    const phrase = KIND_PHRASE[k];
    return props.node.negated ? phrase.negative : phrase.positive;
  };
  const diagnostic = () => {
    const leaf = raw();
    return leaf ? diagnosticFor(props.sheet.query(), leaf) : undefined;
  };
  /** **A property row's edits wait for the registry.** Its operator menu and its
   *  value encoding are both `effectiveTypeOf` answers, and the fallback when
   *  there is no row is the untyped `text` family — so editing while the read is
   *  in flight would silently retype a `number` key, or a key whose declaration
   *  was written a moment ago, as text. Nothing here is a NEW state: the row
   *  keeps its own draft and reads exactly as it did (§6.3, I-20). */
  const propertyPending = () => !!property() && props.sheet.registry.pending();

  const key = (purpose: string) => `${purpose}:${locKey(props.node.loc)}`;
  const menuOpen = (purpose: string) => props.sheet.openMenu() === key(purpose);
  const toggle = (purpose: string) =>
    props.sheet.setOpenMenu(menuOpen(purpose) ? null : key(purpose));

  let fieldTrigger: HTMLButtonElement | undefined;
  let opTrigger: HTMLButtonElement | undefined;
  let valueTrigger: HTMLButtonElement | undefined;
  let rowMenuTrigger: HTMLButtonElement | undefined;
  const fieldMenuId = `qs-field-${createUniqueId()}`;
  const opMenuId = `qs-op-${createUniqueId()}`;
  const rowMenuId = `qs-rowmenu-${createUniqueId()}`;

  /** Replace this row's whole node — the wrappers included, because the negative
   *  operator IS a `not` wrapper (§7.4). */
  const replaceRow = (filter: Filter) =>
    props.sheet.apply(replaceAt(root(), props.node.loc, filter));

  const setPropertyOperator = (id: PropertyOperatorId) => {
    const test = property();
    if (!test || propertyPending()) return;
    const arity = propertyOperatorArity(id);
    const next = encodePropertyLeaf({
      id,
      key: test.key,
      values: test.values.slice(0, arity),
      type: effective().type,
      throughPage: test.throughPage,
    });
    // An identity that needs a value the row does not have yet keeps the row on
    // screen with its editor open rather than committing a leaf that says
    // something the user did not.
    if (next) replaceRow(next);
    props.sheet.setOpenMenu(null);
  };

  const setPropertyValues = (values: string[]) => {
    const test = property();
    if (!test || propertyPending()) return;
    const next = encodePropertyLeaf({
      id: test.id,
      key: test.key,
      values,
      type: effective().type,
      throughPage: test.throughPage,
    });
    if (next) replaceRow(next);
  };

  /** What this row currently tests, in the vocabulary picker's terms, so the
   *  list can mark the row the user is already on. */
  const currentChoice = (): VocabularyChoice | null => {
    const test = property();
    if (test) return { kind: "property", key: test.key, throughPage: test.throughPage };
    const k = kind();
    return k ? { kind: "builtin", leaf: k } : null;
  };

  /** Point this row at a different property key.
   *
   *  The row keeps the identity it already had when the new key's effective
   *  type still offers it — retyping `owner` to `cost` should not silently
   *  discard "is not". When it does not, the type's first identity is taken.
   *  And when neither can be encoded from the values on the row (an operator
   *  that needs a value the row has not got yet), the row commits `is set`,
   *  which is a complete condition the user can then narrow — rather than a
   *  leaf that claims a comparison nobody typed. */
  const setPropertyKey = (choice: VocabularyChoice & { kind: "property" }) => {
    if (props.sheet.registry.pending()) return;
    const target = effectiveFor(props.sheet.registry, choice.key);
    const offered = propertyOperators(target).map((operator) => operator.id);
    const test = property();
    const values = test?.values ?? [];
    const id = test && offered.includes(test.id) ? test.id : offered[0] ?? "is_set";
    const encode = (candidate: PropertyOperatorId) =>
      encodePropertyLeaf({
        id: candidate,
        key: choice.key,
        values,
        type: target.type,
        throughPage: choice.throughPage,
      });
    const next = encode(id) ?? encode("is_set");
    if (next) replaceRow(next);
  };

  const toggleNegated = () => {
    // The positive leaf is what a negative operator wraps, so flipping is
    // wrapping or unwrapping exactly one `not`.
    props.sheet.apply(
      props.node.negated
        ? unwrapAt(root(), props.node.loc)
        : wrapAt(root(), props.node.loc, "not"),
    );
    props.sheet.setOpenMenu(null);
  };

  return (
    <div
      class="qs-row"
      role="listitem"
      classList={{ "qs-row-raw": !!raw(), "qs-off": props.node.disabled }}
    >
      <Show
        when={!raw()}
        fallback={
          <>
            {/* A retained leaf: the decoded text, the diagnostic, and a ×. Red
                when it invalidates the query, greyed when it is disabled — a
                disabled broken row does not invalidate (§3.5). */}
            <span class="qs-raw-text">{raw()!.text}</span>
            <span class="qs-raw-message" role={props.node.disabled ? undefined : "alert"}>
              {diagnostic()?.message ?? "This condition was not understood."}
            </span>
            <Show when={diagnostic()?.suggestions?.length}>
              {/* Rust's OWN alternatives (§4.3.2). The frontend has no
                  fuzzy resolver and does not grow one; this renders the list the
                  parser already sent. */}
              <span class="qs-raw-suggestions">
                Did you mean{" "}
                <For each={diagnostic()!.suggestions!.slice(0, 4)}>
                  {(suggestion, index) => (
                    <>
                      <Show when={index() > 0}>, </Show>
                      <code>{suggestion}</code>
                    </>
                  )}
                </For>
                ?
              </span>
            </Show>
            {/* A retained leaf is exactly the row the sheet cannot edit in
                place, so it is the row that most needs the way out (§7.5). */}
            <Show when={props.sheet.onEditText}>
              <button
                type="button"
                class="qs-raw-edit"
                onClick={(e) => {
                  stop(e);
                  props.sheet.onEditText?.();
                }}
              >
                Edit as text
              </button>
            </Show>
          </>
        }
      >
        <span class="qs-cell qs-cell-field">
          <button
            ref={fieldTrigger}
            type="button"
            class="qs-field"
            aria-haspopup="listbox"
            aria-expanded={menuOpen("field") ? "true" : "false"}
            aria-controls={fieldMenuId}
            disabled={props.node.disabled}
            onClick={(e) => {
              stop(e);
              toggle("field");
            }}
          >
            {fieldLabel()} ▾
          </button>
          <Popover
            open={() => menuOpen("field")}
            close={() => props.sheet.setOpenMenu(null)}
            parentId={props.sheet.layerId}
            trigger={() => fieldTrigger ?? null}
          >
            {(rootRef) => (
              /* The SAME picker the add-condition flow uses (§7.5). Changing a
                 row's field and adding a condition were two controls asking one
                 question; they are one control now, so a key's count and type
                 are visible wherever the question is asked. */
              <QueryVocabularyPicker
                id={fieldMenuId}
                anchor={props.sheet.anchor()}
                rows={props.sheet.registry.rows}
                pending={props.sheet.registry.pending}
                current={currentChoice()}
                rootRef={rootRef}
                onPick={(choice) => {
                  // A property pick needs the key's effective type to encode a
                  // leaf. While the read is in flight the menu stays open with
                  // its own line saying why, rather than committing text.
                  if (choice.kind === "property" && props.sheet.registry.pending()) return;
                  props.sheet.setOpenMenu(null);
                  if (choice.kind === "property") return setPropertyKey(choice);
                  // The filter sheet offers only the filter vocabulary; a
                  // display field cannot reach this picker.
                  if (choice.kind === "field") return;
                  const next = choice.leaf;
                  if (next === "scheduled" || next === "deadline") return replaceRow(planningFilter(next));
                  if (next === "journal") return replaceRow(journalFilter());
                  // Anything that needs a value re-opens the value editor with
                  // the new field selected; the row is not committed until the
                  // value is.
                  props.sheet.setOpenMenu(`value:${locKey(props.node.loc)}:${next}`);
                }}
              />
            )}
          </Popover>
        </span>
        <span class="qs-cell qs-cell-op">
          <button
            ref={opTrigger}
            type="button"
            class="qs-op"
            aria-haspopup="listbox"
            aria-expanded={menuOpen("op") ? "true" : "false"}
            aria-controls={opMenuId}
            disabled={props.node.disabled || propertyPending()}
            title={propertyPending() ? "Reading this graph's properties…" : undefined}
            onClick={(e) => {
              stop(e);
              toggle("op");
            }}
          >
            {operatorLabel()} ▾
          </button>
          <Popover
            open={() => menuOpen("op")}
            close={() => props.sheet.setOpenMenu(null)}
            parentId={props.sheet.layerId}
            trigger={() => opTrigger ?? null}
          >
            {(rootRef) => (
              <Show
                when={property()}
                fallback={
                  <Listbox
                    id={opMenuId}
                    label="Condition operator"
                    rootRef={rootRef}
                    options={(() => {
                      const k = kind();
                      const phrase = k ? KIND_PHRASE[k] : { positive: "matches", negative: "does not match" };
                      return [
                        { key: "positive", label: phrase.positive, active: !props.node.negated },
                        { key: "negative", label: phrase.negative, active: props.node.negated },
                      ];
                    })()}
                    onPick={(picked) => {
                      if ((picked === "negative") !== props.node.negated) toggleNegated();
                      else props.sheet.setOpenMenu(null);
                    }}
                  />
                }
              >
                {(test) => (
                  <Listbox
                    id={opMenuId}
                    label="Condition operator"
                    rootRef={rootRef}
                    options={(() => {
                      const offered = propertyOperators(effective());
                      const listed = offered.map((operator) => ({
                        key: operator.id,
                        label: operator.label,
                        active: operator.id === test().id,
                      }));
                      // A reopen-only identity (P2's atom-level `!=`) is shown as
                      // the current selection so the row says what it IS, even
                      // though the menu does not offer it as a new choice.
                      return listed.some((option) => option.active)
                        ? listed
                        : [
                            {
                              key: test().id,
                              label: propertyOperatorLabel(test().id, effective().cardinality),
                              active: true,
                            },
                            ...listed,
                          ];
                    })()}
                    onPick={(picked) => setPropertyOperator(picked as PropertyOperatorId)}
                  />
                )}
              </Show>
            )}
          </Popover>
        </span>
        <span class="qs-cell qs-cell-value">
          <Show
            when={property()}
            fallback={
              <>
                <button
                  ref={valueTrigger}
                  type="button"
                  class="qs-value"
                  disabled={props.node.disabled}
                  aria-label="Condition value"
                  onClick={(e) => {
                    stop(e);
                    const k = kind();
                    props.sheet.setOpenMenu(
                      menuOpen(`value`) || !k ? null : `value:${locKey(props.node.loc)}:${k}`,
                    );
                  }}
                >
                  {filterValueLabel(core())}
                </button>
                <Popover
                  open={() => (props.sheet.openMenu() ?? "").startsWith(`value:${locKey(props.node.loc)}:`)}
                  close={() => props.sheet.setOpenMenu(null)}
                  parentId={props.sheet.layerId}
                  trigger={() => valueTrigger ?? null}
                >
                  {(rootRef) => (
                    <div ref={rootRef} class="qs-menu" onClick={stop}>
                      <ValueEditor
                        kind={(props.sheet.openMenu() ?? "").split(":").pop() as BuilderLeafKind}
                        onCommit={(filter) => {
                          props.sheet.setOpenMenu(null);
                          replaceRow(props.node.negated ? { kind: "not", inner: filter } : filter);
                        }}
                      />
                    </div>
                  )}
                </Popover>
              </>
            }
          >
            {(test) => (
              <PropertyValueCell
                test={test()}
                effective={effective()}
                disabled={props.node.disabled || propertyPending()}
                registry={props.sheet.registry}
                onCommit={setPropertyValues}
              />
            )}
          </Show>
        </span>
      </Show>
      <span class="qs-menu-wrap">
        <button
          ref={rowMenuTrigger}
          type="button"
          class="qs-row-menu"
          aria-label="Row actions"
          aria-haspopup="listbox"
          aria-expanded={menuOpen("row") ? "true" : "false"}
          aria-controls={rowMenuId}
          onClick={(e) => {
            stop(e);
            toggle("row");
          }}
        >
          ⋮
        </button>
        <Popover
          open={() => menuOpen("row")}
          close={() => props.sheet.setOpenMenu(null)}
          parentId={props.sheet.layerId}
          trigger={() => rowMenuTrigger ?? null}
        >
          {(rootRef) => (
            <Listbox
              id={rowMenuId}
              label="Row actions"
              rootRef={rootRef}
              options={[
                { key: "group", label: "Group with row above" },
                { key: "remove", label: "Remove" },
              ]}
              onPick={(picked) => {
                props.sheet.setOpenMenu(null);
                props.sheet.apply(
                  picked === "group"
                    ? groupWithPrevious(root(), props.node.loc)
                    : removeAt(root(), props.node.loc),
                );
              }}
            />
          )}
        </Popover>
      </span>
      <button
        type="button"
        class="qs-row-remove"
        aria-label="Remove condition"
        title="Remove"
        onClick={(e) => {
          stop(e);
          props.sheet.apply(removeAt(root(), props.node.loc));
        }}
      >
        ×
      </button>
      <Show when={props.node.disabled}>
        <span class="qs-off-label">disabled</span>
      </Show>
    </div>
  );
}

/** A property row's value cell: the key's type surface, and one or two inputs
 *  filled in with the values the leaf already carries. */
function PropertyValueCell(props: {
  test: PropertyLeafTest;
  effective: { type: ObservedType; cardinality: Cardinality };
  disabled: boolean;
  registry: RegistryAccess;
  onCommit: (values: string[]) => void;
}): JSX.Element {
  const arity = () => propertyOperatorArity(props.test.id);
  const [low, setLow] = createSignal(props.test.values[0] ?? "");
  const [high, setHigh] = createSignal(props.test.values[1] ?? "");
  createEffect(() => {
    setLow(props.test.values[0] ?? "");
    setHigh(props.test.values[1] ?? "");
  });
  const commit = () => props.onCommit(arity() === 2 ? [low(), high()] : [low()]);
  return (
    <span class="qs-property-value">
      <span class="qs-property-key">{props.test.key}</span>
      <PropertyType
        propertyKey={props.test.key}
        rows={props.registry.rows}
        onDeclarationWritten={props.registry.request}
      />
      <Show when={arity() > 0}>
        <input
          class="qs-input"
          aria-label={arity() === 2 ? "From" : "Value"}
          placeholder={arity() === 2 ? "From" : "Value"}
          disabled={props.disabled}
          value={low()}
          onInput={(e) => setLow(e.currentTarget.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
          }}
        />
      </Show>
      <Show when={arity() === 2}>
        <input
          class="qs-input"
          aria-label="To"
          placeholder="To"
          disabled={props.disabled}
          value={high()}
          onInput={(e) => setHigh(e.currentTarget.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
          }}
        />
      </Show>
    </span>
  );
}

/** The add-a-condition row: the ONE vocabulary picker, then the value editor for
 *  what was chosen. `/query` opens the sheet with exactly this open and focused
 *  (§7.3).
 *
 *  **It used to ask two questions.** "What kind of condition?" and then, for the
 *  two property kinds, "which key?" — from a facets list with no counts and no
 *  types. It now asks one: the built-in vocabulary and the graph's own property
 *  keys are rows of the same list, so choosing `owner` is one keystroke-filtered
 *  pick rather than a pick, a submenu and a second pick (§7.5). */
function AddCondition(props: {
  sheet: QuerySheetProps;
  open: Accessor<boolean>;
  setOpen: (open: boolean) => void;
  onAdd: (filter: Filter) => void;
}): JSX.Element {
  const [chosen, setChosen] = createSignal<VocabularyChoice | null>(null);
  const [propertyId, setPropertyId] = createSignal<PropertyOperatorId>("is");
  const [propertyValues, setPropertyValues] = createSignal<string[]>([]);
  let triggerEl: HTMLButtonElement | undefined;
  const chooserId = `qs-add-${createUniqueId()}`;
  const reset = () => {
    setChosen(null);
    setPropertyValues([]);
  };
  const close = () => {
    reset();
    props.setOpen(false);
  };
  const property = () => {
    const choice = chosen();
    return choice?.kind === "property" ? choice : null;
  };
  const effective = createMemo(() => effectiveFor(props.sheet.registry, property()?.key));
  /** The chosen key's operators and value encoding are registry answers, so
   *  while the read is in flight the editor keeps the key and the draft on
   *  screen and refuses to commit — rather than encoding a `number` key, or one
   *  whose declaration has just been written, as text (§6.3). */
  const registryPending = () => props.sheet.registry.pending();
  const commitProperty = () => {
    const choice = property();
    if (!choice || registryPending()) return;
    const filter = encodePropertyLeaf({
      id: propertyId(),
      key: choice.key,
      values: propertyValues(),
      type: effective().type,
      throughPage: choice.throughPage,
    });
    if (!filter) return;
    props.onAdd(filter);
    // Enter on a completed value commits the row and reopens the chooser for
    // the next condition (design §2.9).
    reset();
    props.setOpen(true);
  };
  const choose = (choice: VocabularyChoice) => {
    if (choice.kind === "builtin") {
      const next = choice.leaf;
      if (next === "scheduled" || next === "deadline") return props.onAdd(planningFilter(next));
      if (next === "journal") return props.onAdd(journalFilter());
      setChosen(choice);
      return;
    }
    setChosen(choice);
    // The family's first identity is pre-selected the moment the key is chosen,
    // so the common case needs no click (§7.4). Which family it is comes from
    // the registry, so when the read is still in flight the pre-selection waits
    // for it (below) instead of defaulting to text.
    if (!registryPending()) setPropertyId(propertyOperators(effective())[0]?.id ?? "is");
  };
  // The key stays chosen across the wait; the moment its rows land, the family's
  // first identity is selected exactly as it would have been on the pick.
  createEffect(() => {
    if (registryPending() || !property()) return;
    if (!propertyOperators(effective()).some((operator) => operator.id === propertyId())) {
      setPropertyId(propertyOperators(effective())[0]?.id ?? "is");
    }
  });
  return (
    <div class="qs-add-wrap">
      <button
        ref={triggerEl}
        type="button"
        class="qs-add"
        aria-haspopup="listbox"
        aria-expanded={props.open() ? "true" : "false"}
        aria-controls={chooserId}
        onClick={(e) => {
          stop(e);
          props.open() ? close() : props.setOpen(true);
        }}
      >
        + Add condition
      </button>
      <Popover open={props.open} close={close} parentId={props.sheet.layerId} trigger={() => triggerEl ?? null}>
        {(rootRef) => (
          <Show
            when={chosen()}
            fallback={
              <QueryVocabularyPicker
                id={chooserId}
                anchor={props.sheet.anchor()}
                rows={props.sheet.registry.rows}
                pending={props.sheet.registry.pending}
                placeholder="Type to add a condition"
                rootRef={rootRef}
                onPick={choose}
              />
            }
          >
            {(choice) => (
              <div ref={rootRef} class="qs-menu" onClick={stop}>
                <Show
                  when={property()}
                  fallback={
                    <ValueEditor
                      kind={(choice() as VocabularyChoice & { kind: "builtin" }).leaf}
                      onCommit={(filter) => {
                        props.onAdd(filter);
                        reset();
                        props.setOpen(true);
                      }}
                    />
                  }
                >
                  {(pick) => (
                    <div class="qs-value-editor">
                      <div class="qs-menu-title">
                        {pick().key}
                        <Show when={pick().throughPage}>
                          <span class="qs-menu-scope"> (page property)</span>
                        </Show>
                      </div>
                      <PropertyType
                        propertyKey={pick().key}
                        rows={props.sheet.registry.rows}
                        onDeclarationWritten={props.sheet.registry.request}
                      />
                      <Show
                        when={!registryPending()}
                        fallback={
                          /* The key and the draft below stay exactly where the
                             user left them; only the choice of comparison waits,
                             because which comparisons exist is the registry's
                             answer. */
                          <div class="qs-registry-pending" role="status">
                            Reading this graph's properties…
                          </div>
                        }
                      >
                        <Listbox
                          class="qs-inline-list"
                          id={`${chooserId}-op`}
                          label="Condition operator"
                          options={propertyOperators(effective()).map((operator) => ({
                            key: operator.id,
                            label: operator.label,
                            active: operator.id === propertyId(),
                          }))}
                          onPick={(picked) => {
                            setPropertyId(picked as PropertyOperatorId);
                            if (propertyOperatorArity(picked as PropertyOperatorId) === 0) {
                              queueMicrotask(commitProperty);
                            }
                          }}
                        />
                      </Show>
                      <Show when={propertyOperatorArity(propertyId()) > 0}>
                        <input
                          class="qs-input"
                          autofocus
                          aria-label="Value"
                          placeholder="Value"
                          disabled={registryPending()}
                          value={propertyValues()[0] ?? ""}
                          onInput={(e) => setPropertyValues([e.currentTarget.value])}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") commitProperty();
                          }}
                        />
                      </Show>
                      <button
                        type="button"
                        class="qs-commit"
                        disabled={registryPending()}
                        onClick={commitProperty}
                      >
                        Add
                      </button>
                    </div>
                  )}
                </Show>
              </div>
            )}
          </Show>
        )}
      </Popover>
    </div>
  );
}

/** The bounded phrase for a filter — exported so the host's sentence and the
 *  sheet's chip cannot drift apart. */
export { filterPhrase };
