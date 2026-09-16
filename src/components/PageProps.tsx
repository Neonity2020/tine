import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount, type JSX } from "solid-js";
import { pagePropsPanel, closePageProps, type PropsPanelScope } from "../ui";
import {
  blockPageReadOnly,
  blockProperty,
  doc,
  formatForBlock,
  formatForPage,
  pageByName,
  readPageProperty,
  setBlockProperty,
  setPageProperty,
} from "../store";
import {
  PAGE_PROP_SPECS,
  isEditablePropertyKey,
  isSheetCellHidden,
  type PagePropSpec,
} from "../editor/properties";
import { facetsOf } from "../render/facets";
import { pageProperties } from "../render/block";
import { dismissTopTransient, registerTransientLayer } from "../transientLayers";

// Properties panel: labelled fields for the properties of a page's pre-block or
// of one block. Every field reads the current value and writes back through the
// store (undo-safe, persisted via the normal save path). Opened from the page
// title gear, the "/Page properties" command, or a block's context menu.
//
// GH #164 generalized this from five fixed page keys to ANY key the file has,
// plus an add-row. The five presets stay first and keep their labels and hints;
// a property with no preset labels itself with its own key and edits as text.
export function PageProps(): JSX.Element {
  return (
    <Show when={pagePropsPanel()}>
      {(p) => <Panel scope={p().scope} x={p().x} y={p().y} />}
    </Show>
  );
}

// Hidden from the editor: the machine-managed builtins (`id`, `collapsed`,
// `logseq.order-list-type`) plus the `tine.*` keys the Sheets surface owns.
// isSheetCellHidden is exactly that rule — it is named for the sheet-cell editor,
// but the question it answers, "is this property machine-managed?", is the same
// one here, so this delegates rather than minting a second hidden set (D-14).
// Lowercased at the call site: isBuiltinHidden matches an all-lowercase set
// literally, and keys arrive in whatever case the file uses.
function machineManaged(key: string): boolean {
  return isSheetCellHidden(key.toLowerCase());
}

/** The properties the scope actually has, through the canonical plural readers:
 *  `pageProperties` for a page pre-block, `facetsOf(...).properties` for a
 *  block. Neither is re-implemented here. */
function existingProperties(scope: PropsPanelScope): [string, string][] {
  if (scope.kind === "page") {
    const page = pageByName(scope.name);
    return page ? pageProperties(page.preBlock, formatForPage(scope.name)) : [];
  }
  const node = doc.byId[scope.id];
  return node ? facetsOf(node.raw, formatForBlock(scope.id)).properties : [];
}

function readOne(scope: PropsPanelScope, key: string): string | null {
  return scope.kind === "page" ? readPageProperty(scope.name, key) : blockProperty(scope.id, key);
}

function writeOne(scope: PropsPanelScope, key: string, value: string | null): void {
  if (scope.kind === "page") setPageProperty(scope.name, key, value);
  else setBlockProperty(scope.id, key, value);
}

/** Whether this scope may be edited here. Deliberately refuses only when the
 *  subject is LOADED and positively says it is read-only.
 *
 *  Not `pageWritable`: that answers a different question — it is also false for
 *  a page the store has not loaded and for one transiently mid-save, and
 *  conflating those with "read-only" both told the user something untrue and
 *  blanked the whole form while a save was in flight. A page we have not loaded
 *  is unknown, not read-only, and `setPageProperty` is already a no-op for it. */
function scopeWritable(scope: PropsPanelScope): boolean {
  if (scope.kind === "block") return doc.byId[scope.id] ? !blockPageReadOnly(scope.id) : true;
  const page = pageByName(scope.name);
  return !page || (!page.readOnly && !page.guide);
}

function scopeLabel(scope: PropsPanelScope): { title: string; subject: string } {
  if (scope.kind === "page") return { title: "Page properties", subject: scope.name };
  const first = (doc.byId[scope.id]?.raw ?? "").split("\n")[0]?.trim() ?? "";
  return { title: "Block properties", subject: first.length > 48 ? `${first.slice(0, 48)}…` : first };
}

/** Rows = the five presets (page scope only, shown even when absent) followed by
 *  every other property the scope actually has. Presets stay FIRST so the first
 *  `.pp-input` remains a preset field. An undeclared key renders as plain text —
 *  `PagePropSpec` gains no new `kind`. */
function rowsFor(scope: PropsPanelScope): PagePropSpec[] {
  const declared = scope.kind === "page" ? PAGE_PROP_SPECS : [];
  const seen = new Set(declared.map((spec) => spec.key.toLowerCase()));
  const rows = [...declared];
  for (const [key] of existingProperties(scope)) {
    const lower = key.toLowerCase();
    if (seen.has(lower) || machineManaged(lower)) continue;
    seen.add(lower);
    rows.push({ key, label: key, hint: "", kind: "text" });
  }
  return rows;
}

const sameKeys = (a: PagePropSpec[], b: PagePropSpec[]) =>
  a.length === b.length && a.every((row, i) => row.key === b[i].key);

function Panel(props: { scope: PropsPanelScope; x: number; y: number }): JSX.Element {
  const w = typeof window !== "undefined" ? window.innerWidth : 1280;
  const h = typeof window !== "undefined" ? window.innerHeight : 800;
  const left = Math.max(8, Math.min(props.x, w - 332));
  // Anchor at the click, then once mounted lift the panel up by its measured
  // height so its full content stays on-screen — no scrollbar for normal content.
  const [top, setTop] = createSignal(Math.max(8, Math.min(props.y, h - 380)));
  let el: HTMLDivElement | undefined;
  createEffect(() => {
    const unregister = registerTransientLayer({ id: "page-properties", root: () => el ?? null, dismiss: () => { closePageProps(); return true; } });
    onCleanup(unregister);
  });
  onMount(() => setTop(Math.max(8, Math.min(props.y, h - (el?.offsetHeight ?? 380) - 8))));
  // Keyed by the KEY SET, not by row identity: an unrelated store change (an
  // external reload, a save) must not remount the fields and discard whatever
  // the user is halfway through typing.
  const rows = createMemo(() => rowsFor(props.scope), undefined, { equals: sameKeys });
  const writable = createMemo(() => scopeWritable(props.scope));
  const heading = createMemo(() => scopeLabel(props.scope));
  return (
    <div
      class="pp-overlay"
      onClick={closePageProps}
      onContextMenu={(e) => {
        e.preventDefault();
        closePageProps();
      }}
    >
      <div ref={el} class="page-props-panel" style={{ left: `${left}px`, top: `${top()}px` }} onClick={(e) => e.stopPropagation()}>
        <div class="pp-head">
          {heading().title} <span class="pp-page">{heading().subject}</span>
        </div>
        <Show
          when={writable()}
          fallback={<div class="pp-hint">This {props.scope.kind} is read-only, so its properties cannot be changed here.</div>}
        >
          <For each={rows()}>{(spec) => <Field scope={props.scope} spec={spec} />}</For>
          <AddRow scope={props.scope} />
        </Show>
        <div class="pp-foot">
          <button class="pp-done" onClick={closePageProps}>Done</button>
        </div>
      </div>
    </div>
  );
}

function Field(props: { scope: PropsPanelScope; spec: PagePropSpec }): JSX.Element {
  const initial = readOne(props.scope, props.spec.key) ?? "";

  if (props.spec.kind === "bool") {
    const [on, setOn] = createSignal(initial.toLowerCase() === "true");
    return (
      <label class="pp-field pp-bool">
        <input
          type="checkbox"
          checked={on()}
          onChange={(e) => {
            setOn(e.currentTarget.checked);
            writeOne(props.scope, props.spec.key, e.currentTarget.checked ? "true" : null);
          }}
        />
        <span class="pp-text">
          <span class="pp-label">{props.spec.label}</span>
          <span class="pp-hint">{props.spec.hint}</span>
        </span>
      </label>
    );
  }

  const [v, setV] = createSignal(initial);
  // Only write on an actual local edit. Otherwise blurring/closing the panel
  // re-commits the value read when it opened — clobbering a concurrent external
  // edit (OG/Syncthing) that the file-watcher reloaded while the panel was open.
  const commit = () => {
    if (v() === initial) return;
    writeOne(props.scope, props.spec.key, v().trim() || null);
  };
  // An undeclared key has no preset, so it is removable from here; the presets
  // are cleared by emptying their field, as they always were.
  const removable = !PAGE_PROP_SPECS.some((spec) => spec.key === props.spec.key);
  return (
    <div class="pp-field">
      <div class="pp-row-head">
        <label class="pp-label">{props.spec.label}</label>
        <Show when={removable}>
          <button
            class="pp-remove"
            title={`Remove ${props.spec.key}`}
            onClick={() => writeOne(props.scope, props.spec.key, null)}
          >
            Remove
          </button>
        </Show>
      </div>
      <input
        class="pp-input"
        value={v()}
        placeholder={props.spec.kind === "list" ? "comma, separated" : ""}
        onInput={(e) => setV(e.currentTarget.value)}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.isComposing || e.keyCode === 229) return;
          if (e.key === "Enter") {
            commit();
            closePageProps();
          } else if (e.key === "Escape") {
            if (dismissTopTransient("escape")) e.preventDefault();
          }
        }}
        onBlur={commit}
      />
      <Show when={props.spec.hint}>
        <div class="pp-hint">{props.spec.hint}</div>
      </Show>
    </div>
  );
}

function AddRow(props: { scope: PropsPanelScope }): JSX.Element {
  const [key, setKey] = createSignal("");
  const [value, setValue] = createSignal("");
  // The key is validated by the matcher that will later have to FIND it, never
  // by a local regex (GH #164): anything accepted here is matchable for update
  // and removal. A machine-managed key is refused — those have their own surface.
  const trimmed = () => key().trim();
  const valid = createMemo(() => isEditablePropertyKey(trimmed()) && !machineManaged(trimmed()));
  const commit = () => {
    if (!valid()) return;
    writeOne(props.scope, trimmed(), value().trim() || null);
    setKey("");
    setValue("");
  };
  const onKeyDown = (e: KeyboardEvent) => {
    e.stopPropagation();
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter") commit();
    else if (e.key === "Escape") {
      if (dismissTopTransient("escape")) e.preventDefault();
    }
  };
  return (
    <div class="pp-field pp-add">
      <label class="pp-label">Add a property</label>
      <div class="pp-add-row">
        <input
          class="pp-input pp-add-key"
          value={key()}
          placeholder="key"
          onInput={(e) => setKey(e.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <input
          class="pp-input pp-add-value"
          value={value()}
          placeholder="value"
          onInput={(e) => setValue(e.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <button class="pp-add-commit" disabled={!valid()} onClick={commit}>Add</button>
      </div>
      <div class="pp-hint">
        Any key you like, written to the file as an ordinary property. Leave the
        value empty to write the key with no value.
      </div>
    </div>
  );
}
