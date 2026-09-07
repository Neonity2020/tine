import { For, Show, createEffect, createMemo, createSignal, type JSX } from "solid-js";
import { createVirtualizer } from "@tanstack/solid-virtual";
import type { Anchor, ObservedType, RegistryRow } from "../editor/queryIr";
import type { BuilderLeafKind } from "../editor/queryBuilder";
import { effectiveTypeOf, registryRowFor, typePhrase } from "./PropertyType";
import { Listbox, type ListboxBody, type ListboxOption } from "./QueryListbox";

// **The one vocabulary picker (SPEC §7.5, §6.4, I-13, I-22).**
//
// Before this there were two choosers and neither showed the graph. Adding a
// condition asked for a TYPE first ("Property"), then for a KEY, from
// `query_facets` — a whole-graph key/value scan whose answer carried no counts,
// no types and no ordering, so the list was alphabetical noise in which a key
// used 400 times sat below one used once. Changing a row's field asked the same
// question through a second control.
//
// This is one list, and it is the registry's (§6.1): the observed type, the
// authoritative count and the common values the engine has already computed.
// Three properties it is responsible for:
//
//  - **Observation and coercion are different things, and both are labelled.**
//    The row says what the graph LOOKS like (`observed`) and, when the key
//    carries a `tine.type` declaration, what the engine will COERCE by
//    (`declared`). Operators and value encoding still come from
//    `effectiveTypeOf` — the display never decides a comparison (§6.3).
//  - **A count is authoritative or it is absent.** Built-in task markers and
//    page predicates are not registry rows; the registry holds no statistics for
//    them, and inventing one — or borrowing a same-named property's — would be a
//    number the user could act on and the engine never said. Their rows carry
//    no second line at all.
//  - **Bounded rendering (I-22).** A graph with thousands of property keys
//    mounts a viewport's worth of rows plus overscan, through
//    `@tanstack/solid-virtual`. The keyboard, the roving active option and the
//    `aria-activedescendant` wiring are `QueryListbox`'s, unchanged (D-14, N1);
//    this file replaces the list BODY and the data adapter, nothing else.

/** What choosing a vocabulary row means, in the builder's own terms. */
export type VocabularyChoice =
  | { kind: "builtin"; leaf: BuilderLeafKind }
  /** An ordinary property filter. `throughPage` is `og.rs::through_page`: a
   *  block row reading its owning PAGE's property (§7.4, F5). */
  | { kind: "property"; key: string; throughPage: boolean };

export type VocabularySection = "builtin" | "property" | "page" | "novel";

export interface VocabularyEntry {
  /** Stable row identity — the option id, never a filtered or virtual index. */
  id: string;
  section: VocabularySection;
  label: string;
  choice: VocabularyChoice;
  icon: string;
  /** The registry's observed type phrase, shown as an OBSERVATION. */
  observed?: string;
  /** The declared (and therefore effective) type phrase, shown as such. */
  declared?: string;
  /** The authoritative count in this scope. **Absent is not zero:** a built-in
   *  has no registry statistics, and saying "0" would be a claim. */
  count?: number;
  unit: "blocks" | "pages";
  /** A short preview of the registry's `top_values`. */
  preview?: string;
}

export const SECTION_LABEL: Record<VocabularySection, string> = {
  builtin: "Built-in",
  property: "Properties in this graph",
  page: "Page",
  // Deliberately not "not in this graph": one of these rows is offered for a
  // key the graph HAS on the other kind of owner, and while the registry read
  // is in flight nobody knows which case it is.
  novel: "Use a key by name",
};

/** The block-side built-in vocabulary, in `FILTER_TYPES` order (§7.4). */
const BLOCK_BUILTINS: { kind: BuilderLeafKind; label: string; icon: string }[] = [
  { kind: "page", label: "Page / tag reference", icon: "↗" },
  { kind: "task", label: "Task marker", icon: "☑" },
  { kind: "priority", label: "Priority", icon: "!" },
  { kind: "scheduled", label: "Scheduled", icon: "◷" },
  { kind: "deadline", label: "Deadline", icon: "◷" },
  { kind: "between", label: "Between dates", icon: "◷" },
  { kind: "content", label: "Full-text search", icon: "⌕" },
];

/** The page-side built-in vocabulary, in `FILTER_TYPES` order (§7.4). */
const PAGE_BUILTINS: { kind: BuilderLeafKind; label: string; icon: string }[] = [
  { kind: "journal", label: "On journal page", icon: "◷" },
  { kind: "onPage", label: "On page", icon: "▤" },
  { kind: "namespace", label: "In namespace", icon: "▤" },
  { kind: "pageTags", label: "Page tags", icon: "↗" },
];

const TYPE_ICON: Record<ObservedType, string> = {
  text: "T",
  number: "#",
  date: "◷",
  checkbox: "☑",
  ref: "↗",
};

const matches = (needle: string, ...haystacks: (string | undefined)[]) =>
  !needle || haystacks.some((text) => (text ?? "").toLowerCase().includes(needle));

/** At most three of the registry's eight `top_values`, as `a · b · c`. */
function previewOf(row: RegistryRow): string | undefined {
  const values = (row.top_values ?? []).slice(0, 3).map(([value]) => value).filter(Boolean);
  return values.length ? values.join(" · ") : undefined;
}

function propertyEntry(
  row: RegistryRow,
  section: VocabularySection,
  throughPage: boolean,
  count: number,
  unit: "blocks" | "pages",
): VocabularyEntry {
  const effective = effectiveTypeOf(row);
  return {
    id: `${section}:${row.normalized_name}`,
    section,
    label: row.normalized_name,
    choice: { kind: "property", key: row.normalized_name, throughPage },
    icon: TYPE_ICON[effective.type],
    observed: typePhrase(row.observed_type, row.cardinality),
    declared: row.declared ? typePhrase(row.declared[0], row.declared[1]) : undefined,
    count,
    unit,
    preview: previewOf(row),
  };
}

/**
 * **The vocabulary, for one anchor, filtered by one needle — pure (§7.5).**
 *
 * Ordering inside the registry sections is by DESCENDING count with a stable
 * `normalized_name` tie-break, so the key a graph actually uses is the key the
 * list opens on. The built-in sections keep their fixed §7.4 order: they have no
 * counts to sort by, and reordering the fixed vocabulary between graphs would
 * move the entry a returning user is reaching for.
 *
 * The count SCOPE follows the anchor (F5). A `@block` query counts blocks for
 * its own properties and pages for the page-owned ones it reads through
 * `page`; a `@page` query counts pages. A property may honestly appear in both
 * sections — an ordinary key is often written on blocks and on pages.
 */
export function buildVocabulary(input: {
  rows: RegistryRow[] | undefined;
  anchor: Anchor;
  search?: string;
}): VocabularyEntry[] {
  const needle = (input.search ?? "").trim().toLowerCase();
  const rows = input.rows ?? [];
  const byCount = (pick: (row: RegistryRow) => number) => (a: RegistryRow, b: RegistryRow) =>
    pick(b) - pick(a) || a.normalized_name.localeCompare(b.normalized_name);

  const builtin = BLOCK_BUILTINS.filter((entry) => matches(needle, entry.label, entry.kind)).map(
    (entry): VocabularyEntry => ({
      id: `builtin:${entry.kind}`,
      section: "builtin",
      label: entry.label,
      choice: { kind: "builtin", leaf: entry.kind },
      icon: entry.icon,
      unit: input.anchor === "page" ? "pages" : "blocks",
    }),
  );

  // The anchor row's OWN properties: `props` on the row the query selects.
  const ownUnit = input.anchor === "page" ? ("pages" as const) : ("blocks" as const);
  const ownCount = (row: RegistryRow) =>
    input.anchor === "page" ? row.count_pages : row.count_blocks;
  const own = rows
    .filter((row) => ownCount(row) > 0 && matches(needle, row.normalized_name))
    .sort(byCount(ownCount))
    .map((row) => propertyEntry(row, "property", false, ownCount(row), ownUnit));

  const pageBuiltins = PAGE_BUILTINS.filter((entry) => matches(needle, entry.label, entry.kind)).map(
    (entry): VocabularyEntry => ({
      id: `builtin:${entry.kind}`,
      section: "page",
      label: entry.label,
      choice: { kind: "builtin", leaf: entry.kind },
      icon: entry.icon,
      unit: "pages",
    }),
  );
  // Only a BLOCK row can read a property "through" its page; on a page anchor
  // the page's own properties are already the section above.
  const pageProperties =
    input.anchor === "block"
      ? rows
          .filter((row) => row.count_pages > 0 && matches(needle, row.normalized_name))
          .sort(byCount((row) => row.count_pages))
          .map((row) => propertyEntry(row, "page", true, row.count_pages, "pages"))
      : [];

  return [...builtin, ...own, ...pageBuiltins, ...pageProperties];
}

/**
 * **The rows for a key the user TYPED, one per scope it is not already in.**
 *
 * A query may name a property before the graph has one, and — the case that got
 * lost when the two-stage chooser went away — it may name a key the graph has
 * on the OTHER kind of owner. `PropertyKeyPick` used to be reached through two
 * separate field kinds, `property` and `pageProperty`, so any typed key could be
 * authored either way; one list keyed off `registryRowFor` gave back neither. A
 * key written only on pages was unauthorable as a block property, and a brand
 * new key was authorable only as a block one.
 *
 * So the offer is per SCOPE, and it is made exactly where the scope's own
 * section does not already carry the key:
 *
 *  - `@block` offers the block's own property and the owning page's property;
 *  - `@page` has one scope, so it offers one choice (F5: there is no reading a
 *    page's property "through" a page).
 *
 * **Counts are the registry's or they are absent.** A key with a row and
 * `count_blocks == 0` is honestly `0 blocks today` in the block scope — that is
 * the registry's own number, and the scope label says which question it answers.
 * A key with no row at all is 0 for the same reason. But while `rows` is
 * `undefined` the read has not landed, and 0 would be a claim about a graph
 * nobody has looked at yet, so the row carries NO count and both scopes are
 * offered — a pending registry is not a known-empty one.
 *
 * Duplicate detection stays `registryRowFor`, the existing matcher, because
 * normalizing a key is the engine's job and a second normalizer here would
 * disagree with it on exactly the keys that matter (D-14).
 */
export function novelKeyEntries(
  rows: RegistryRow[] | undefined,
  search: string,
  anchor: Anchor,
): VocabularyEntry[] {
  const key = search.trim();
  if (!key) return [];
  const loaded = rows !== undefined;
  const row = registryRowFor(rows, key);
  const scopes: { throughPage: boolean; unit: "blocks" | "pages"; count: number }[] =
    anchor === "page"
      ? [{ throughPage: false, unit: "pages", count: row?.count_pages ?? 0 }]
      : [
          { throughPage: false, unit: "blocks", count: row?.count_blocks ?? 0 },
          { throughPage: true, unit: "pages", count: row?.count_pages ?? 0 },
        ];
  return scopes
    .filter((scope) => !loaded || scope.count === 0)
    .map((scope) => ({
      id: `novel:${scope.throughPage ? "page" : "own"}:${key}`,
      section: "novel" as const,
      label: `Use "${key}" as a ${scope.unit === "pages" ? "page" : "block"} property`,
      // The user's OWN spelling is what a query edits with; punctuation, quotes
      // and non-ASCII stay bound IR values and are never concatenated into text.
      choice: { kind: "property" as const, key, throughPage: scope.throughPage },
      icon: "+",
      // The key may be one the graph HAS, just not here — say what it looks
      // like, since the registry already knows.
      observed: row ? typePhrase(row.observed_type, row.cardinality) : undefined,
      declared: row?.declared ? typePhrase(row.declared[0], row.declared[1]) : undefined,
      count: loaded ? scope.count : undefined,
      unit: scope.unit,
    }));
}

/** Section headers interleaved into the flat option list the listbox draws. */
function withHeaders(entries: VocabularyEntry[]): ListboxOption[] {
  const options: ListboxOption[] = [];
  let section: VocabularySection | null = null;
  for (const entry of entries) {
    if (entry.section !== section) {
      section = entry.section;
      options.push({ key: `section:${section}`, label: SECTION_LABEL[section], header: true });
    }
    options.push({ key: entry.id, label: entry.label });
  }
  return options;
}

/** The estimated height of one row, in CSS pixels. Production sizing for the
 *  virtualizer — there is no test-only branch below it; the picker's own tests
 *  stub the VIEWPORT geometry jsdom does not have, and the screenshots are what
 *  prove the real layout (N2). */
export const VOCABULARY_ROW_HEIGHT = 52;
export const VOCABULARY_COMPACT_ROW_HEIGHT = 32;
export const VOCABULARY_HEADER_HEIGHT = 26;

/** The vocabulary item's own name, whichever kind it is: `content` for a
 *  built-in field, `status` for a property. It used to read `builtin:content`
 *  for one kind and the bare key for the other, so an automation — or a person
 *  reading the DOM — that asked for a field by name found every property and
 *  missed every built-in. The option's internal id stays prefixed, because THAT
 *  has to be unique across sections; this attribute answers a different
 *  question and now answers it the same way for both.
 */
export function vocabularyKey(entry: VocabularyEntry | undefined, fallback: string): string {
  if (!entry) return fallback;
  return entry.choice.kind === "property" ? entry.choice.key : entry.choice.leaf;
}

/** Which SCOPE a property row authors — `og.rs::through_page` as an attribute,
 *  so the two rows a typed key can produce are told apart by what they mean and
 *  not by their position in the list. Absent on a built-in, which has no scope
 *  to choose. */
export function vocabularyThroughPage(entry: VocabularyEntry | undefined): string | undefined {
  return entry && entry.choice.kind === "property" ? String(entry.choice.throughPage) : undefined;
}

/** Whether a row has a second line at all. **One predicate, used twice:** the
 *  virtualizer sizes the slot from it and the row is drawn from it, so the
 *  measured list and the painted list cannot disagree — a windowed list whose
 *  estimates and CSS drift apart overlaps its own rows. */
export function hasDetail(entry: VocabularyEntry | undefined): boolean {
  return !!entry && !!(entry.observed || entry.declared || entry.count !== undefined || entry.preview);
}

export function QueryVocabularyPicker(props: {
  id: string;
  label?: string;
  placeholder?: string;
  anchor: Anchor;
  rows: () => RegistryRow[] | undefined;
  /** The one registry read has not landed for the current graph/revision. The
   *  list still offers everything that needs no registry; what it must not do
   *  is look like a graph with no properties in it. */
  pending?: () => boolean;
  /** What the row currently tests, so the list can mark it. */
  current?: VocabularyChoice | null;
  onPick: (choice: VocabularyChoice) => void;
  rootRef?: (element: HTMLDivElement) => void;
  onEmptyBackspace?: () => void;
}): JSX.Element {
  const [search, setSearch] = createSignal("");
  const entries = createMemo(() => [
    ...buildVocabulary({ rows: props.rows(), anchor: props.anchor, search: search() }),
    ...novelKeyEntries(props.rows(), search(), props.anchor),
  ]);
  const byId = createMemo(() => new Map(entries().map((entry) => [entry.id, entry])));
  const options = createMemo(() => withHeaders(entries()));
  const same = (choice: VocabularyChoice, other: VocabularyChoice | null | undefined) =>
    !!other
    && other.kind === choice.kind
    && (choice.kind === "builtin"
      ? other.kind === "builtin" && other.leaf === choice.leaf
      : other.kind === "property"
        && other.key === choice.key
        && other.throughPage === choice.throughPage);

  const body = (context: ListboxBody) => (
    <VirtualList
      context={context}
      entry={(key) => byId().get(key)}
      current={() => props.current ?? null}
      same={same}
    />
  );

  return (
    <Listbox
      id={props.id}
      label={props.label ?? "Condition field"}
      class="qs-vocab"
      filterable
      placeholder={props.placeholder ?? "Search fields and properties"}
      query={search()}
      onQuery={setSearch}
      options={options()}
      status={
        <Show when={props.pending?.()}>
          {/* Compact, one line, above the list: the built-ins below it are
              usable now, and the graph's own keys are on their way. */}
          <div class="qs-vocab-pending" role="status">
            Reading this graph's properties…
          </div>
        </Show>
      }
      rootRef={props.rootRef}
      onEmptyBackspace={props.onEmptyBackspace}
      body={body}
      onPick={(key) => {
        const entry = byId().get(key);
        if (entry) props.onPick(entry.choice);
      }}
    />
  );
}

/** The virtualized body. Mounts the viewport plus overscan — and, always, the
 *  ACTIVE row, so `aria-activedescendant` can never name an unmounted option
 *  while the keyboard is on a row the scroll has not reached yet. */
function VirtualList(props: {
  context: ListboxBody;
  entry: (key: string) => VocabularyEntry | undefined;
  current: () => VocabularyChoice | null;
  same: (choice: VocabularyChoice, other: VocabularyChoice | null) => boolean;
}): JSX.Element {
  let scrollEl: HTMLDivElement | undefined;
  const context = props.context;
  const shown = () => context.shown();
  const virtualizer = createVirtualizer({
    get count() {
      return shown().length;
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: (index) => {
      const option = shown()[index];
      if (!option) return VOCABULARY_ROW_HEIGHT;
      if (option.header) return VOCABULARY_HEADER_HEIGHT;
      // A built-in is one line — it has no registry statistics to show — so it
      // costs one line. Charging every row for a second line put the graph's
      // OWN vocabulary, which is what this list exists for, below the fold.
      return hasDetail(props.entry(option.key))
        ? VOCABULARY_ROW_HEIGHT
        : VOCABULARY_COMPACT_ROW_HEIGHT;
    },
    overscan: 8,
    getItemKey: (index) => shown()[index]?.key ?? index,
  });
  const activeIndex = createMemo(() => {
    const key = context.activeKey();
    return key ? shown().findIndex((option) => option.key === key) : -1;
  });
  // Keyboard navigation scrolls, which is what makes a distant entry reachable
  // at all in a windowed list.
  createEffect(() => {
    const index = activeIndex();
    if (index >= 0) virtualizer.scrollToIndex(index, { align: "auto" });
  });
  const window_ = createMemo(() => {
    const items = virtualizer.getVirtualItems();
    const index = activeIndex();
    if (index < 0 || items.some((item) => item.index === index)) return items;
    const option = shown()[index];
    if (!option) return items;
    return [...items, { index, key: option.key, start: virtualizer.getTotalSize(), size: 0, end: 0, lane: 0 }]
      .slice()
      .sort((a, b) => a.index - b.index);
  });
  return (
    <div ref={scrollEl} class="qs-vocab-viewport">
      <div
        id={context.listId}
        class="qs-options qs-vocab-options"
        role="listbox"
        aria-label={context.label}
        style={{ height: `${virtualizer.getTotalSize()}px`, position: "relative" }}
      >
        <For each={window_()}>
          {(item) => {
            const option = () => shown()[item.index];
            const entry = () => {
              const current = option();
              return current && !current.header ? props.entry(current.key) : undefined;
            };
            return (
              <Show when={option()} keyed>
                {(current) => (
                  <div
                    class="qs-vocab-slot"
                    style={{
                      position: "absolute",
                      top: 0,
                      left: 0,
                      width: "100%",
                      transform: `translateY(${item.start}px)`,
                    }}
                  >
                    <Show
                      when={!current.header}
                      fallback={
                        <div class="qs-option-section" role="presentation">
                          {current.label}
                        </div>
                      }
                    >
                      <button
                        type="button"
                        id={context.optionId(current.key)}
                        class="qs-option qs-vocab-option"
                        classList={{
                          active: current.key === context.activeKey(),
                          current: !!entry() && props.same(entry()!.choice, props.current()),
                          "qs-vocab-novel": entry()?.section === "novel",
                          "is-compact": !hasDetail(entry()),
                        }}
                        role="option"
                        aria-selected={current.key === context.activeKey()}
                        data-section={entry()?.section}
                        data-vocabulary-key={vocabularyKey(entry(), current.key)}
                        data-through-page={vocabularyThroughPage(entry())}
                        onMouseEnter={() => context.setActiveKey(current.key)}
                        onClick={() => context.pick(current.key)}
                      >
                        <span class="qs-vocab-icon" aria-hidden="true">
                          {entry()?.icon ?? "·"}
                        </span>
                        <span class="qs-vocab-body">
                          <span
                            class="qs-vocab-label"
                            classList={{
                              // The code face is for a KEY. A "use this key by
                              // name" row is a sentence about one, so it reads
                              // as prose — the whole line was monospace the
                              // moment those rows became property choices.
                              "is-key":
                                entry()?.choice.kind === "property" && entry()?.section !== "novel",
                            }}
                          >
                            {current.label}
                          </span>
                          <Show when={entry()}>
                            {(row) => <EntryDetail entry={row()} />}
                          </Show>
                        </span>
                      </button>
                    </Show>
                  </div>
                )}
              </Show>
            );
          }}
        </For>
      </div>
    </div>
  );
}

/** The row's second line: what the graph LOOKS like, what the engine COERCES
 *  by, how many owners there are, and a few of the values. Every one of them is
 *  labelled with which question it answers — an unlabelled `text` badge beside a
 *  numeric operator menu is the misreading this wording exists to prevent. */
function EntryDetail(props: { entry: VocabularyEntry }): JSX.Element {
  // **An absent count is not a zero, and it is not a caption either.** A
  // built-in task marker or page predicate is not a registry row: the registry
  // holds no statistics for it, so the row says its name and stops. Printing
  // "no count" under all eleven of them would be eleven lines of nothing, and
  // it reads as a number that failed to load rather than a question the
  // registry was never asked.
  const countText = () => {
    const count = props.entry.count;
    if (count === undefined) return null;
    return `${count} ${props.entry.unit === "pages" ? (count === 1 ? "page" : "pages") : count === 1 ? "block" : "blocks"}`;
  };
  return (
    <Show when={hasDetail(props.entry)}>
    <span class="qs-vocab-meta">
      <Show when={props.entry.observed}>
        {(phrase) => <span class="qs-vocab-type">observed {phrase()}</span>}
      </Show>
      <Show when={props.entry.declared}>
        {(phrase) => <span class="qs-vocab-type is-declared">declared {phrase()}</span>}
      </Show>
      <Show when={countText()}>
        {(text) => (
          <span class="qs-vocab-count">
            {text()}
            <Show when={props.entry.count === 0}> today</Show>
          </span>
        )}
      </Show>
      <Show when={props.entry.preview}>
        {(values) => <span class="qs-vocab-values">{values()}</span>}
      </Show>
    </span>
    </Show>
  );
}
