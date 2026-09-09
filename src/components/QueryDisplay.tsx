import { For, Show, createEffect, createMemo, createSignal, createUniqueId, onCleanup, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import type { AggFn, RegistryRow, ViewSettings } from "../editor/queryIr";
import {
  canonicalGroupField,
  groupingFromViewValue,
  viewAfterViewSwitch,
  type QueryDisplayControl,
} from "../editor/queryViewProperties";
import {
  fieldLabel,
  isFieldId,
  queryAggregateFieldName,
  queryColumnName,
  queryColumnFieldId,
  querySortFieldName,
  type FieldId,
} from "../sheet/fields";
import { QueryVocabularyPicker, type VocabularyEntry } from "./QueryVocabularyPicker";
import { dismissOnOutsidePointer, registerTransientLayer } from "../transientLayers";
import { registerVisiblePopover, type RegistryAccess } from "./QuerySheet";

// **The inline Display panel (P5B).**
//
// A query block persists six display facts, and before this the inline surface
// could state one and a half of them: `+ sort` wrote ONE sort pair, `+ summarize`
// wrote ONE aggregate and one grouping, and the column list, the view kind and
// the sample had no inline control at all. Both of those controls read
// `aggregates[0]` and `sort[0]` and wrote a one-element list back, so a note that
// carried two of either silently lost the rest the first time anyone touched the
// pill.
//
// This panel edits the six as what they are: two enums, three ORDERED LISTS and
// a number. Every list keeps its order, its repeats and the entries this panel
// cannot represent, because it edits `ViewSettings` and hands it to the host's
// one writer (`QueryDisplayControl.apply` → `queryViewPropertyPatch`) rather
// than reaching for a property of its own.

const VIEW_KINDS = ["search", "list", "table", "board"] as const;
type ViewKindName = (typeof VIEW_KINDS)[number];
const VIEW_LABEL: Record<ViewKindName, string> = {
  search: "Search",
  list: "List",
  table: "Table",
  board: "Board",
};

/** The six sheet builtins, as display fields. */
const BUILTIN_FIELDS: readonly FieldId[] = ["state", "priority", "scheduled", "deadline", "tags", "page"];

/** The largest `tine.sample` the IR can carry: `ViewSettings.sample` is a `u32`
 *  in Rust, and a bigger number would be refused by the parse rather than by
 *  the control the user typed it into. */
export const MAX_SAMPLE = 4294967295;

export type SampleReading =
  | { kind: "unset" }
  | { kind: "value"; value: number }
  | { kind: "invalid"; message: string };

/** What a typed sample means. Empty is "no limit"; **zero is a real limit** and
 *  means no rows at all, which the panel says out loud rather than quietly
 *  treating as unset. */
export function readSample(text: string): SampleReading {
  const trimmed = text.trim();
  if (!trimmed) return { kind: "unset" };
  if (!/^\d+$/.test(trimmed)) return { kind: "invalid", message: "A sample is a whole number of rows." };
  const value = Number(trimmed);
  if (value > MAX_SAMPLE) {
    return { kind: "invalid", message: `The largest sample is ${MAX_SAMPLE}.` };
  }
  return { kind: "value", value };
}

/** Which display slot a field vocabulary is being built for. Each slot spells a
 *  field differently, and offering a field a slot cannot carry would be a
 *  control that looks like it saved. */
export type DisplaySlot = "group" | "sort" | "column" | "aggregate";

/** Which ROW a display vocabulary is being built for.
 *
 *  A page and a block are different rows with different attributes, and the
 *  query IR says so (`queryIr.ts::Attr`). Offering a block builtin as a page
 *  field would be a control that writes a setting nothing can honour — and
 *  offering it silently is worse than not offering it, because the author reads
 *  the empty column as "this page has no tasks". */
export type DisplayRowKind = "page" | "block";

/** The page-row builtins a display slot can spell.
 *
 *  Pages have a name, a kind and a journal day, and that is the whole list: the
 *  task marker, priority and the two planning dates belong to a BLOCK, and a
 *  formula is evaluated over block rows. `page` is the block vocabulary's
 *  spelling for "the page this row is on"; for a page row the same question is
 *  its own name, so the page vocabulary spells it `name`. */
const PAGE_BUILTINS: readonly { field: string; label: string }[] = [
  { field: "name", label: "Name" },
  { field: "kind", label: "Kind" },
  { field: "day", label: "Journal day" },
];

/** The display fields a slot can actually take.
 *
 *   * **group** — every field identity, canonically: the six builtins, any
 *     property as `prop:<key>`, any formula as `formula:<name>`.
 *   * **sort** — only what `query.rs::sort_key` can order by: `priority`,
 *     `page`, `scheduled`, `deadline`, and any property NAME. `state`, `tags`,
 *     the title and formulas are sortable by the table over the rows it has,
 *     but the note cannot carry them, so they are not offered here.
 *   * **column** — a `tine.columns` token: the six builtins by name, every
 *     other name an ordinary property. Formulas have no token.
 *   * **aggregate** — a `tine.col-aggregates` key: a LITERAL property name.
 *     Counting rows needs no field at all and summing a task marker is not a
 *     thing, so no builtin and no formula is offered — but a property NAMED like
 *     a builtin is, because that grammar reserves nothing (`sheet/fields.ts::
 *     queryAggregateFieldName`). A key the segment grammar cannot spell is not
 *     offered: picking it would write a segment that reads back as something
 *     else.
 */
export function displayFieldEntries(input: {
  slot: DisplaySlot;
  rows: readonly RegistryRow[] | undefined;
  formulas: readonly string[];
  search: string;
  /** Absent means blocks, which is the vocabulary every existing caller asks
   *  for and gets unchanged. */
  rowKind?: DisplayRowKind;
}): VocabularyEntry[] {
  const search = input.search.trim().toLowerCase();
  const page = input.rowKind === "page";
  const matches = (label: string) => !search || label.toLowerCase().includes(search);
  const out: VocabularyEntry[] = [];
  const push = (value: string, label: string, icon: string, count?: number) => {
    if (!matches(label)) return;
    out.push({
      id: `${input.slot}:${value}`,
      section: "field",
      label,
      choice: { kind: "field", field: value },
      icon,
      // Applicability is counted on the row the vocabulary is FOR: a property
      // on 400 blocks and 2 pages is a poor page column and a good block one.
      unit: page ? "pages" : "blocks",
      ...(count === undefined ? {} : { count }),
    });
  };
  if (page) {
    // No task state, priority, planning or formula: none of them is a page
    // attribute, and none of the writers can spell one for a page row.
    if (input.slot === "group" || input.slot === "column" || input.slot === "sort") {
      for (const builtin of PAGE_BUILTINS) {
        if (input.slot === "column" && builtin.field === "name") continue; // the row's own link
        push(input.slot === "group" ? `prop:${builtin.field}` : builtin.field, builtin.label, "◆");
      }
    }
  } else {
    if (input.slot === "group" || input.slot === "column") {
      for (const field of BUILTIN_FIELDS) {
        if (input.slot === "column" && field === "page") continue; // the table's own breadcrumb
        push(input.slot === "group" ? field : field, fieldLabel(field), "◆");
      }
    }
    if (input.slot === "sort") {
      for (const field of ["priority", "page", "scheduled", "deadline"] as const) {
        push(field, fieldLabel(field), "◆");
      }
    }
  }
  for (const row of input.rows ?? []) {
    const key = row.normalized_name;
    // An ordinary property is distinct from a builtin, and it is offered only
    // where the field grammar preserves that identity — the same gate for both
    // row kinds, because both write the same property spellings.
    if (input.slot === "column" && queryColumnName(`prop:${key}`) === null) continue;
    if (input.slot === "sort" && querySortFieldName(`prop:${key}`) === null) continue;
    if (input.slot === "aggregate" && queryAggregateFieldName(`prop:${key}`) === null) continue;
    // Applicability, on the row this vocabulary is for. A property observed on
    // no page of this graph is not a page field, so the page vocabulary drops
    // it; the block vocabulary keeps its established combined count, because
    // narrowing it would retire block choices this packet has no business
    // retiring.
    if (page && row.count_pages === 0) continue;
    const value = input.slot === "group" ? `prop:${key}` : key;
    push(value, key, "•", page ? row.count_pages : row.count_blocks + row.count_pages);
  }
  if (!page && input.slot === "group") {
    for (const name of input.formulas) push(`formula:${name}`, name, "ƒ");
  }
  return out;
}

/** One field-picking popover, parented to the panel so Escape closes the
 *  innermost open thing first.
 *
 *  **Portalled, like the panel it opens from.** `.qd-panel` is a capped scroll
 *  box, and a scrolling ancestor clips an absolutely positioned popover: the
 *  first draft nested the vocabulary picker inside `.qb-picker` — itself a
 *  scroll box — and every option row laid out at a real position, painted
 *  nowhere and answered no click. All four field choices in the panel come
 *  through here, so that was the whole panel's field vocabulary, unreachable.
 *  The anchor below carries no box of its own; the picker inside it keeps the
 *  sheet menus' geometry, placed in viewport coordinates the same way the panel
 *  is. */
function FieldPicker(props: {
  label: string;
  slot: DisplaySlot;
  rowKind: DisplayRowKind;
  registry: RegistryAccess;
  formulas: () => readonly string[];
  parentTransientId: string;
  current?: string | null;
  onPick: (field: string) => void;
  trigger: (open: () => void, ref: (element: HTMLButtonElement) => void) => JSX.Element;
}): JSX.Element {
  const [open, setOpen] = createSignal(false);
  let triggerEl: HTMLButtonElement | undefined;
  let pickerEl: HTMLDivElement | undefined;
  const layerId = `query-display-field-${createUniqueId()}`;
  const listId = `query-display-list-${createUniqueId()}`;
  registerVisiblePopover(open, {
    id: layerId,
    parentId: props.parentTransientId,
    root: () => pickerEl ?? null,
    trigger: () => triggerEl ?? null,
    dismiss: () => {
      setOpen(false);
      return true;
    },
  });
  createEffect(() => {
    if (open()) props.registry.request();
  });

  const [rect, setRect] = createSignal<{ top: number; left: number } | null>(null);
  /** Below the trigger when the list fits under it, above it when it does not,
   *  and clamped to the window on both axes — the same rule the panel uses, so
   *  a picker opened from the panel's last section is not half off the screen. */
  const measure = () => {
    const element = triggerEl;
    if (!element) return;
    const box = element.getBoundingClientRect();
    if (typeof window === "undefined") {
      setRect({ top: box.bottom, left: box.left });
      return;
    }
    const margin = 8;
    const menu = pickerEl?.firstElementChild as HTMLElement | null;
    const height = menu?.offsetHeight || 380;
    const width = menu?.offsetWidth || 320;
    const top =
      box.bottom + height + margin <= window.innerHeight
        ? box.bottom
        : Math.max(margin, Math.min(box.top - height - margin, window.innerHeight - height - margin));
    const left = Math.max(margin, Math.min(box.left, window.innerWidth - width - margin));
    setRect({ top, left });
  };
  createEffect(() => {
    if (!open()) return;
    measure();
    if (typeof window === "undefined") return;
    window.addEventListener("scroll", measure, true);
    window.addEventListener("resize", measure);
    onCleanup(() => {
      window.removeEventListener("scroll", measure, true);
      window.removeEventListener("resize", measure);
    });
  });

  return (
    <span class="qb-add-wrap">
      {props.trigger(
        () => setOpen(!open()),
        (element) => {
          triggerEl = element;
        },
      )}
      <Show when={open()}>
        <Portal>
          <div
            ref={(element) => {
              pickerEl = element;
              // Measured once the list is really up: its height decides which
              // side of the trigger it can take.
              measure();
            }}
            class="qd-field-picker"
            // The panel asks the DOM whether one of its own pickers is open
            // before it treats a press as "outside"; portalled, this is the
            // only place it can be found. Exact: only the panel that owns this
            // picker is held still by it.
            data-transient-parent={props.parentTransientId}
            style={rect() ? { top: `${rect()!.top}px`, left: `${rect()!.left}px` } : undefined}
            onClick={(e) => e.stopPropagation()}
          >
            <QueryVocabularyPicker
            id={listId}
            label={props.label}
            placeholder="Search fields"
            anchor={props.rowKind}
            rows={props.registry.rows}
            pending={props.registry.pending}
            failure={props.registry.failure}
            onRetry={props.registry.retry}
            current={props.current ? { kind: "field", field: props.current } : null}
            entries={(search) =>
              displayFieldEntries({
                slot: props.slot,
                rowKind: props.rowKind,
                rows: props.registry.rows(),
                formulas: [...props.formulas()],
                search,
              })
            }
            onPick={(choice) => {
              if (choice.kind !== "field") return;
              props.onPick(choice.field);
              setOpen(false);
            }}
            />
          </div>
        </Portal>
      </Show>
    </span>
  );
}

/** A reorder/remove strip shared by the three ordered lists, so all three move
 *  the same way and none of them grows its own idea of what "up" means. */
function OrderedRow(props: {
  label: string;
  index: number;
  length: number;
  move: (from: number, to: number) => void;
  remove: () => void;
  children?: JSX.Element;
}): JSX.Element {
  return (
    <div class="qd-row">
      <span class="qd-row-label">{props.label}</span>
      {props.children}
      <button
        class="qd-row-btn"
        title="Move up"
        disabled={props.index === 0}
        onClick={() => props.move(props.index, props.index - 1)}
      >
        ↑
      </button>
      <button
        class="qd-row-btn"
        title="Move down"
        disabled={props.index === props.length - 1}
        onClick={() => props.move(props.index, props.index + 1)}
      >
        ↓
      </button>
      <button class="qd-row-btn qd-row-remove" title="Remove" onClick={props.remove}>
        ✕
      </button>
    </div>
  );
}

function moved<T>(list: readonly T[], from: number, to: number): T[] {
  const next = [...list];
  if (to < 0 || to >= next.length) return next;
  const [item] = next.splice(from, 1);
  next.splice(to, 0, item);
  return next;
}

const AGG_LABEL: Record<AggFn, string> = { count: "Count", sum: "Sum", avg: "Average" };
const AGG_CYCLE: readonly AggFn[] = ["count", "sum", "avg"];

export function QueryDisplay(props: {
  control: QueryDisplayControl;
  registry: RegistryAccess;
  /** The block's formula field names, which are grouping identities but have no
   *  spelling in any of the other three lists. */
  formulas?: () => readonly string[];
  /** Which result family this panel controls. Absent means blocks — the single
   *  family every caller had before mixed results, so an unchanged caller keeps
   *  its trigger, its dialog name and its whole vocabulary. */
  rowKind?: DisplayRowKind;
  parentTransientId?: string;
  onOpenChange?: (open: boolean) => void;
}): JSX.Element {
  const [open, setOpen] = createSignal(false);
  createEffect(() => props.onOpenChange?.(open()));
  onCleanup(() => props.onOpenChange?.(false));
  const view = () => props.control.view;
  const apply = (next: ViewSettings) => props.control.apply(next);
  const formulas = () => (props.rowKind === "page" ? [] : props.formulas?.() ?? []);
  const rowKind = (): DisplayRowKind => props.rowKind ?? "block";
  /** The accessible names of this panel's trigger and dialog. Two panels can be
   *  mounted side by side on one mixed result, so "Display settings" would name
   *  both of them and neither would say which section it changes. */
  const triggerName = () => (rowKind() === "page" ? "Display pages" : "Display blocks");
  const dialogName = () => (rowKind() === "page" ? "Page display" : "Block display");

  let triggerEl: HTMLButtonElement | undefined;
  let panelEl: HTMLDivElement | undefined;
  // Unique per MOUNT: the same block can be open in two panes, and a
  // block-derived id would silently unregister the first panel's layer.
  const layerId = `query-display:${createUniqueId()}`;
  createEffect(() => {
    if (!open()) return;
    const unregister = registerTransientLayer({
      id: layerId,
      parentId: props.parentTransientId,
      root: () => panelEl ?? null,
      trigger: () => triggerEl ?? null,
      dismiss: () => {
        setOpen(false);
        return true;
      },
    });
    onCleanup(unregister);
  });
  dismissOnOutsidePointer({
    open,
    // A press while one of the panel's own field pickers is open belongs to
    // that picker's rung; collapsing two rungs on one press is what makes a
    // ladder feel broken.
    inside: () =>
      document.querySelector(`[data-transient-parent="${layerId}"]`)
        ? [document.body]
        : [panelEl ?? null, triggerEl ?? null],
    dismiss: () => setOpen(false),
  });

  const [rect, setRect] = createSignal<{ top: number; left: number; width: number } | null>(null);
  /** Where the panel goes, in viewport coordinates.
   *
   *  Below the trigger when it fits, above it when it does not, and pinned to
   *  the top margin when neither side has room. A panel placed below a trigger
   *  low on a tall window ran off the bottom edge, and it does not scroll the
   *  page — the sections past the fold were simply unreachable, which is the
   *  half of the settings that had no inline control before this panel. The
   *  narrow bottom-sheet layout overrides all of this in CSS (`top: auto
   *  !important`), so it is unaffected. */
  const measure = () => {
    const element = triggerEl;
    if (!element) return;
    const box = element.getBoundingClientRect();
    const width = Math.max(box.width, 320);
    if (typeof window === "undefined") {
      setRect({ top: box.bottom, left: box.left, width });
      return;
    }
    const margin = 8;
    // The panel is capped and scrolls (`.qd-panel`), so its rendered height IS
    // the most it can take; measure it rather than re-deriving the CSS cap.
    const height = panelEl?.offsetHeight || Math.min(window.innerHeight * 0.7, 560);
    const top =
      box.bottom + height + margin <= window.innerHeight
        ? box.bottom
        : Math.max(margin, Math.min(box.top - height - margin, window.innerHeight - height - margin));
    const left = Math.max(margin, Math.min(box.left, window.innerWidth - width - margin));
    setRect({ top, left, width });
  };
  createEffect(() => {
    if (!open()) return;
    measure();
    if (typeof window === "undefined") return;
    window.addEventListener("scroll", measure, true);
    window.addEventListener("resize", measure);
    onCleanup(() => {
      window.removeEventListener("scroll", measure, true);
      window.removeEventListener("resize", measure);
    });
  });

  // ---- the six facts ------------------------------------------------------
  const currentView = (): ViewKindName =>
    (VIEW_KINDS as readonly string[]).includes(view().view ?? "") ? (view().view as ViewKindName) : "list";
  // The Board's grouping default is `viewAfterViewSwitch`'s, shared with the
  // header switcher — one click, one meaning, wherever it is made.
  const setView = (next: ViewKindName) =>
    apply(viewAfterViewSwitch(view(), next === "list" ? undefined : next, rowKind()));

  const grouping = createMemo(() =>
    groupingFromViewValue(viewAfterViewSwitch(view(), view().view, rowKind()).group_by));
  const groupLabel = () => {
    const resolved = grouping();
    if (resolved.kind === "cleared") return "No grouping";
    if (resolved.kind === "unset") return "Not set";
    return isFieldId(resolved.field) ? fieldLabel(resolved.field) : resolved.field;
  };
  const setGroup = (field: string | null) =>
    apply({ ...view(), group_by: field === null ? "" : canonicalGroupField(field) ?? "" });

  const sorts = () => view().sort ?? [];
  const setSorts = (next: ViewSettings["sort"]) => apply({ ...view(), sort: next });
  const columns = () => view().columns ?? [];
  const unavailableColumns = () => [
    ...(props.registry.rows() ?? []).filter((row) => queryColumnName(`prop:${row.normalized_name}`) === null)
      .map((row) => `${row.normalized_name} (property)`),
    ...formulas().map((name) => `${name} (formula)`),
  ];
  const setColumns = (next: string[]) => apply({ ...view(), columns: next });
  const aggregates = () => view().aggregates ?? [];
  /** The segments of the same property this panel keeps and cannot edit — the
   *  host reads them from the block's own bytes through the one parser. */
  const retainedAggregates = () => props.control.retainedAggregates ?? [];
  const setAggregates = (next: ViewSettings["aggregates"]) => apply({ ...view(), aggregates: next });

  const [sampleText, setSampleText] = createSignal("");
  // Re-seeded from the view whenever the panel opens, so a half-typed number
  // from a previous visit never looks like the note's value.
  createEffect(() => {
    if (open()) setSampleText(view().sample == null ? "" : String(view().sample));
  });
  const sample = createMemo(() => readSample(sampleText()));
  const commitSample = () => {
    const reading = sample();
    if (reading.kind === "invalid") return;
    const next = reading.kind === "unset" ? undefined : reading.value;
    if ((view().sample ?? undefined) === next) return;
    apply({ ...view(), sample: next });
  };

  const summary = () => {
    const parts: string[] = [VIEW_LABEL[currentView()]];
    if (grouping().kind === "field") parts.push(`by ${groupLabel()}`);
    if (sorts().length) parts.push(`${sorts().length} sort${sorts().length > 1 ? "s" : ""}`);
    if (aggregates().length) parts.push(`${aggregates().length} Σ`);
    return parts.join(" · ");
  };

  const panel = () => (
    <div
      ref={(element) => {
        panelEl = element;
        // Measured once it is really up: its height decides which side of the
        // trigger it can take, and before the portal mounts there is none.
        measure();
      }}
      class="qs-sheet qd-panel"
      role="dialog"
      aria-label={dialogName()}
      // The host sheet asks the DOM whether one of its own popovers is open
      // before it treats a press as "outside". This panel is PORTALLED out of
      // the sheet, so it cannot be found under the sheet's element — it carries
      // its parent layer's id instead, which is exact: only the sheet that owns
      // this panel is held still by it.
      data-transient-parent={props.parentTransientId}
      onClick={(e) => e.stopPropagation()}
    >
      <div class="qd-section">
        <div class="qd-section-title">View</div>
        <div class="qd-views" role="group" aria-label="Query view">
          <For each={VIEW_KINDS}>
            {(kind) => (
              <button
                type="button"
                class="qd-view"
                classList={{ active: currentView() === kind }}
                onClick={() => setView(kind)}
              >
                {VIEW_LABEL[kind]}
              </button>
            )}
          </For>
        </div>
      </div>

      <div class="qd-section">
        <div class="qd-section-title">Group by</div>
        <div class="qd-row">
          <span class="qd-row-label">{groupLabel()}</span>
          <FieldPicker
            label="Group by field"
            slot="group"
            rowKind={rowKind()}
            registry={props.registry}
            formulas={formulas}
            parentTransientId={layerId}
            current={grouping().kind === "field" ? (grouping() as { field: string }).field : null}
            onPick={(field) => setGroup(field)}
            trigger={(toggle, ref) => (
              <button ref={ref} class="qd-row-btn" title="Choose a grouping field" onClick={toggle}>
                Change
              </button>
            )}
          />
          {/* An explicit clear, distinct from "nothing said": it is what stops a
              switch to Board from reinstating the task marker. */}
          <button
            class="qd-row-btn"
            classList={{ active: grouping().kind === "cleared" }}
            title="Show one ungrouped set"
            onClick={() => setGroup(null)}
          >
            None
          </button>
        </div>
      </div>

      <div class="qd-section">
        <div class="qd-section-title">Sort</div>
        <For each={sorts()}>
          {([field, dir], index) => (
            <OrderedRow
              label={field}
              index={index()}
              length={sorts().length}
              move={(from, to) => setSorts(moved(sorts(), from, to))}
              remove={() => setSorts(sorts().filter((_, i) => i !== index()))}
            >
              <button
                class="qd-row-btn"
                title={dir === "asc" ? "Ascending" : "Descending"}
                onClick={() =>
                  setSorts(
                    sorts().map((entry, i) =>
                      i === index() ? [entry[0], entry[1] === "asc" ? "desc" : "asc"] : entry,
                    ),
                  )
                }
              >
                {/* A WORD, not an arrow. The move-up/move-down strip beside it
                    is two arrows already, and a third arrow next to them says
                    nothing about which of the three it is. */}
                {dir === "asc" ? "Asc" : "Desc"}
              </button>
            </OrderedRow>
          )}
        </For>
        <FieldPicker
          label="Sort field"
          slot="sort"
          rowKind={rowKind()}
          registry={props.registry}
          formulas={formulas}
          parentTransientId={layerId}
          onPick={(field) => setSorts([...sorts(), [field, "asc"]])}
          trigger={(toggle, ref) => (
            <button ref={ref} class="qd-add" onClick={toggle}>
              + sort
            </button>
          )}
        />
      </div>

      <div class="qd-section">
        <div class="qd-section-title">Columns</div>
        <Show when={columns().length === 0}>
          <div class="qd-empty">All fields the rows carry</div>
        </Show>
        <Show when={unavailableColumns().length > 0}>
          <div class="qd-note qd-column-limits">
            Custom column selections cannot include {unavailableColumns().join(", ")}. Use default columns to show them.
          </div>
        </Show>
        <For each={columns()}>
          {(name, index) => (
            <OrderedRow
              label={fieldLabel(queryColumnFieldId(name))}
              index={index()}
              length={columns().length}
              move={(from, to) => setColumns(moved(columns(), from, to))}
              remove={() => setColumns(columns().filter((_, i) => i !== index()))}
            />
          )}
        </For>
        <FieldPicker
          label="Column"
          slot="column"
          rowKind={rowKind()}
          registry={props.registry}
          formulas={formulas}
          parentTransientId={layerId}
          onPick={(field) => setColumns([...columns(), field])}
          trigger={(toggle, ref) => (
            <button ref={ref} class="qd-add" onClick={toggle}>
              + column
            </button>
          )}
        />
      </div>

      <div class="qd-section">
        <div class="qd-section-title">Summarize</div>
        <For each={aggregates()}>
          {([field, fn], index) => (
            <OrderedRow
              label={field || "Whole result"}
              index={index()}
              length={aggregates().length}
              move={(from, to) => setAggregates(moved(aggregates(), from, to))}
              remove={() => setAggregates(aggregates().filter((_, i) => i !== index()))}
            >
              <button
                class="qd-row-btn"
                title="Change function"
                onClick={() =>
                  setAggregates(
                    aggregates().map((entry, i) =>
                      i === index()
                        ? [entry[0], AGG_CYCLE[(AGG_CYCLE.indexOf(entry[1]) + 1) % AGG_CYCLE.length]]
                        : entry,
                    ),
                  )
                }
              >
                {AGG_LABEL[fn]}
              </button>
            </OrderedRow>
          )}
        </For>
        <div class="qd-add-row">
          <button class="qd-add" onClick={() => setAggregates([...aggregates(), ["", "count"]])}>
            + count
          </button>
          <FieldPicker
            label="Aggregate property"
            slot="aggregate"
            rowKind={rowKind()}
            registry={props.registry}
            formulas={formulas}
            parentTransientId={layerId}
            onPick={(field) => setAggregates([...aggregates(), [field, "sum"]])}
            trigger={(toggle, ref) => (
              <button ref={ref} class="qd-add" onClick={toggle}>
                + property
              </button>
            )}
          />
        </div>
        {/* `tine.col-aggregates` is shared ground (contract §5): the sheet
            footer's own vocabulary rides in the same property, every save
            preserves it, and none of these controls can edit it. Saying so is
            the difference between retained and quietly gone. */}
        <Show when={retainedAggregates().length}>
          <div class="qd-note qd-retained">
            Kept from the table, not editable here: {retainedAggregates().join(", ")}
          </div>
        </Show>
      </div>

      <div class="qd-section">
        <div class="qd-section-title">Sample</div>
        <input
          class="qb-input qd-sample"
          inputmode="numeric"
          aria-label="Sample size"
          placeholder="No limit"
          value={sampleText()}
          onInput={(e) => setSampleText(e.currentTarget.value)}
          onBlur={commitSample}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Enter") commitSample();
          }}
        />
        <Show when={sample().kind === "invalid"}>
          <div class="qd-error" role="alert">
            {(sample() as { message: string }).message}
          </div>
        </Show>
        {/* Zero is a limit, not a blank: say what it does before it is saved. */}
        <Show when={sample().kind === "value" && (sample() as { value: number }).value === 0}>
          <div class="qd-note">No results (sample 0)</div>
        </Show>
      </div>
    </div>
  );

  return (
    <span class="qb-add-wrap">
      <button
        ref={triggerEl}
        class="qb-sort qd-trigger"
        classList={{ active: open() }}
        title={`${triggerName()} — view, grouping, sorts, columns, summaries and sample`}
        aria-label={triggerName()}
        aria-haspopup="dialog"
        aria-expanded={open() ? "true" : "false"}
        onClick={(e) => {
          e.stopPropagation();
          setOpen(!open());
        }}
      >
        {`display: ${summary()}`}
      </button>
      <Show when={open()}>
        <Portal>
          <div
            class="qs-overlay"
            onClick={(e) => {
              e.stopPropagation();
              setOpen(false);
            }}
          />
          <div
            class="qs-sheet-anchor"
            style={
              rect()
                ? { top: `${rect()!.top}px`, left: `${rect()!.left}px`, width: `${rect()!.width}px` }
                : undefined
            }
          >
            {panel()}
          </div>
        </Portal>
      </Show>
    </span>
  );
}
