import type { Format, PageDto } from "../types";
import { backend } from "../backend";
import { blockWritable, bumpCollapseEpoch, bumpCollapseEpochs, doc, formatForBlock, freshId, markDirty, pageByName, pageWritable, setDoc } from "./doc";
import { ensurePageLoaded } from "./lifecycle";
import { facetsOf } from "../render/facets";
import { graphBinding } from "../persistence";
import { PROP_LINE, isBuiltinHidden, isPageHeaderPropertiesOnly, isPropertiesOnly, joinProps, markdownRawWithProperty, orgPreBlockWithProperty, orgRawWithProperty, readPropertyValue, splitPagePreamble, splitProps, upsertPropertyLine } from "../editor/properties";
import { produce } from "solid-js/store";
import { pushToast } from "../ui";
import { pushUndo, withUndoUnit } from "./undo";

let selectedIdsImpl: (() => string[]) | null = null;

export function registerSelectedIds(fn: () => string[]): void {
  selectedIdsImpl = fn;
}


/** Current value of a block property, read through the ONE lsdoc-backed
 *  recognizer (facetsOf) — a raw line scan here returned property-lookalikes
 *  from code fences/body text and silently suppressed real config writes
 *  (review finding). Case-insensitive key match, like OG. */
export function blockProperty(id: string, key: string): string | null {
  const node = doc.byId[id];
  if (!node) return null;
  const lower = key.toLowerCase();
  for (const [k, v] of facetsOf(node.raw, formatForBlock(id)).properties) {
    if (k.toLowerCase() === lower) return v.trim();
  }
  return null;
}

/** Whether a block lives on a read-only page (the org round-trip gate) — sheet
 *  write paths outside the block editor must consult this before mutating. */
export function blockPageReadOnly(id: string): boolean {
  const n = doc.byId[id];
  return n ? (pageByName(n.page)?.readOnly ?? false) : false;
}

/** Set (or remove, when value is null) a `key:: value` block property. Property
 *  lines live immediately after the first line, before body text, matching OG's
 *  block-property placement and keeping every property writer on one path. */
export function setBlockProperty(id: string, key: string, value: string | null) {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return;
  pushUndo(`prop:${id}:${key}`, [node.page]);
  if (formatForBlock(id) === "org") {
    // ORG blocks carry properties in a `:PROPERTIES:` drawer — writing a
    // markdown `key:: value` line into org renders as visible body text and is
    // NOT read back as a property (same class as GH #25 for id::). Mirrors
    // rawWithBlockId's canonical placement: title, planning, drawer, body.
    setDoc("byId", id, "raw", orgRawWithProperty(node.raw, key, value));
    markDirty(node.page);
    return;
  }
  // Canonical head-region placement plus legacy trailing-property cleanup lives
  // in the shared pure writer so compound mutations (heading transitions) can
  // remain one undo-safe raw rewrite.
  setDoc("byId", id, "raw", markdownRawWithProperty(node.raw, key, value));
  markDirty(node.page);
}

/** Read a page-level property from the page's pre-block (the leading
 *  `key:: value` lines), or null. */
export function readPageProperty(pageName: string, key: string): string | null {
  const p = doc.pages.find((x) => x.name === pageName);
  if (!p) return null;
  const fromPreBlock = readPropertyValue(p.preBlock, key);
  if (fromPreBlock !== null) return fromPreBlock;
  const first = p.format === "md" ? doc.byId[p.roots[0]] : null;
  return first && isPropertiesOnly(first.raw) ? readPropertyValue(first.raw, key) : null;
}

/** Set or clear a page-level property in the page's canonical property source:
 *  pre-block normally, or OG's properties-only first bullet. Persists through
 *  the normal dirty/save path and is undo-safe. */
export function setPageProperty(pageName: string, key: string, value: string | null) {
  const idx = doc.pages.findIndex((x) => x.name === pageName);
  if (idx < 0 || !pageWritable(pageName)) return;
  pushUndo(`pageprop:${pageName}:${key}`, [pageName]);
  const page = doc.pages[idx];
  const first = page.format === "md" ? doc.byId[page.roots[0]] : null;
  // A properties-only first root is the same editable source as the rendered
  // header. Do not silently duplicate its property into preBlock; pageToDto or
  // the native new-header boundary canonicalizes its persisted form.
  if (first && (first.originatedFromPageHeader || (!page.preBlock && isPropertiesOnly(first.raw)))) {
    const next = upsertPropertyLine(first.raw, key, value) ?? "";
    if (first.originatedFromPageHeader && next === "") {
      setDoc(produce((s) => {
        const target = s.pages.find((p) => p.name === pageName);
        if (target?.roots[0] === first.id) target.roots.shift();
        delete s.byId[first.id];
      }));
    } else {
      setDoc("byId", first.id, "raw", next);
    }
    markDirty(pageName);
    return;
  }
  // The preamble's form is the file's, not the caller's convenience: org pages
  // carry page properties as `#+key:` directives, and a markdown `key:: value`
  // line written into an org preamble is body text to org and to Logseq, so the
  // property silently would not exist (I-4, GH #164).
  setDoc(
    "pages",
    idx,
    "preBlock",
    page.format === "org"
      ? orgPreBlockWithProperty(doc.pages[idx].preBlock, key, value)
      : upsertPropertyLine(doc.pages[idx].preBlock, key, value)
  );
  markDirty(pageName);
}

/** Write (or clear, with `null`) a page property on the page named `pageName`,
 *  CREATING that page first if the graph has none — the §6.3 "declare type…"
 *  action's one write.
 *
 *  A property key's declaration lives on the page whose name IS the key, and
 *  most keys have no page yet. There is no `create_page` command and inventing
 *  one would be a second write path (I-1, D-14): a page is created exactly as
 *  {@link captureToPage} creates one, by loading the backend's answer for the
 *  name — or a synthetic empty DTO when there is none — and letting the ordinary
 *  dirty/save path write the file. So the bytes that land on disk are an
 *  ordinary Logseq page with an ordinary `key:: value` line (I-4).
 *
 *  The load is async and {@link withUndoUnit} is not, which is why they are not
 *  combined: creation completes FIRST, and the one undo entry is the property
 *  write, which {@link setPageProperty} already pushes.
 *
 *  Returns whether the property was written. Clearing a declaration on a page
 *  that does not exist is a no-op, not a page creation. */
export async function ensurePagePropertyOnKeyPage(
  pageName: string,
  prop: string,
  value: string | null,
): Promise<boolean> {
  const name = pageName.trim();
  if (!name) return false;
  if (!pageByName(name)) {
    if (value === null) return false;
    const binding = graphBinding();
    const dto: PageDto =
      (await backend().getPage(name, "page"))
      ?? { name, kind: "page", title: name, pre_block: null, blocks: [], rev: null };
    // A refusal means the name slot now holds a DIFFERENT page than the one this
    // request was made against; writing into it would put the declaration on the
    // wrong page (the `captureOutlineInto` rule, GH #254).
    if (await ensurePageLoaded(dto, { expectedGraphBinding: binding })) return false;
  }
  if (!pageByName(name) || !pageWritable(name)) return false;
  setPageProperty(name, prop, value);
  return true;
}

/** Materialize an existing canonical Markdown page header as Tine's ordinary
 * first-root editor. This is representation-only: no undo entry, dirty flag or
 * save is created until the user actually changes the node. */
export function beginPageHeaderEdit(pageName: string): string | null {
  const page = pageByName(pageName);
  if (!page || page.format !== "md" || !pageWritable(pageName)) return null;
  const first = doc.byId[page.roots[0]];
  if (first && (first.originatedFromPageHeader || (!page.preBlock && isPropertiesOnly(first.raw)))) {
    return first.id;
  }

  const split = splitPagePreamble(page.preBlock);
  if (!split.properties || !isPageHeaderPropertiesOnly(split.properties)) return null;
  const id = freshId();
  setDoc(
    produce((s) => {
      const index = s.pages.findIndex((p) => p.name === pageName);
      s.pages[index].preBlock = split.remainder;
      s.byId[id] = {
        id,
        raw: split.properties!,
        collapsed: false,
        parent: null,
        page: pageName,
        children: [],
        originatedFromPageHeader: true,
      };
      s.pages[index].roots.unshift(id);
    })
  );
  return id;
}

/** Remove a deleted transient header root after its editor exits. Invalid
 * drafts intentionally remain present and editable; pageToDto keeps them from
 * reaching native persistence. */
export function finishPageHeaderEdit(id: string): void {
  const node = doc.byId[id];
  if (!node?.originatedFromPageHeader || node.raw !== "" || node.children.length > 0) return;
  setDoc(
    produce((s) => {
      const page = s.pages.find((p) => p.name === node.page);
      if (page?.roots[0] === id) page.roots.shift();
      delete s.byId[id];
    })
  );
}

/** The one wording for "this page header is not valid properties yet", shared by
 * the projection that refuses to serialize it and the editor-exit check that
 * tells the user. Two copies of a rule drift; the copy that loses its rationale
 * is where the next bug lands. */
export const PAGE_HEADER_INVALID_TOAST =
  "Page-header properties must contain only valid key:: value lines before they can be saved.";

/** Adopt the page header that a COMPLETED save folded into the file's preamble.
 *
 * `projectPageDto` folds a flagless properties-only first bullet into
 * `pre_block` (GH #198). That rewrites the file, but the store keeps holding
 * those properties as an ordinary first root with an empty `preBlock`, so from
 * that moment the store and the file disagree about where the page header
 * lives. The next keystroke that leaves the bullet transiently NOT
 * properties-only then proposes `pre_block: null` plus a property-bearing
 * outline block — exactly what the data-preservation firewall refuses
 * (GH #163). That refusal carries no typed code, so it reaches the user as
 * `reason code: unknown`, is classified retryable, and after three tries
 * becomes a red toast while the block is still being edited (GH #546).
 *
 * Marking the root as the page header hands the page to the header path, which
 * folds it exactly and defers while it is invalid instead of contradicting the
 * file. Compare-and-swap on the exact folded text: the save was in flight, so
 * the user may have typed since, and a later edit must not be adopted silently.
 */
export function adoptFoldedPageHeader(pageName: string, folded: string): void {
  const page = pageByName(pageName);
  if (!page || page.preBlock) return;
  const first = doc.byId[page.roots[0]];
  if (!first || first.originatedFromPageHeader || first.children.length > 0) return;
  if (first.raw.replace(/\n+$/, "") !== folded) return;
  setDoc(
    produce((s) => {
      const node = s.byId[first.id];
      if (node) node.originatedFromPageHeader = true;
    })
  );
}

/** Tell the user their page header is not valid properties — when the editor
 * CLOSES, which is the moment they finished writing it (GH #546).
 *
 * Autosave deliberately says nothing: it fires shortly after a typing pause, so
 * a half-written header is the ordinary state of an unfinished edit. The edit
 * is never lost — the projection refuses to serialize an invalid header, the
 * page stays dirty, and it saves as soon as the properties are valid again. */
export function reportInvalidPageHeaderOnExit(id: string): void {
  const node = doc.byId[id];
  if (!node?.originatedFromPageHeader) return;
  // An empty draft deletes the header, and `finishPageHeaderEdit` has already
  // removed it — nothing to complain about.
  if (node.raw === "") return;
  if (node.children.length === 0 && isPageHeaderPropertiesOnly(node.raw.replace(/\n+$/, ""))) return;
  pushToast(PAGE_HEADER_INVALID_TOAST, "error");
}

/** Turn ordinary text before the first Markdown bullet into a real first block
 * only when the user chooses to edit it (GH #85). Until then the preamble stays
 * byte-preserved and an unrelated save cannot silently add an outline marker. */
export function promotePagePreamble(pageName: string): string | null {
  const page = pageByName(pageName);
  if (!page || page.format !== "md" || !pageWritable(pageName)) return null;
  const { properties, content } = splitPagePreamble(page.preBlock);
  if (!content) return null;
  pushUndo(`promote-preamble:${pageName}`, [pageName]);
  const id = freshId();
  setDoc(
    produce((s) => {
      const index = s.pages.findIndex((p) => p.name === pageName);
      s.pages[index].preBlock = properties;
      s.byId[id] = { id, raw: content, collapsed: false, parent: null, page: pageName, children: [] };
      const markedHeader = s.byId[s.pages[index].roots[0]]?.originatedFromPageHeader;
      s.pages[index].roots.splice(markedHeader ? 1 : 0, 0, id);
    })
  );
  markDirty(pageName);
  return id;
}

/** Toggle a property: set it to `value`, or remove it if already that value. */
export function toggleBlockProperty(id: string, key: string, value: string) {
  setBlockProperty(id, key, blockProperty(id, key) === value ? null : value);
}

const ORDER_KEY = "logseq.order-list-type";
function isOrdered(id: string | null | undefined): boolean {
  return !!id && blockProperty(id, ORDER_KEY) === "number";
}

function orderListTypeFromRaw(raw: string, format: Format): string | null {
  for (const [key, value] of facetsOf(raw, format).properties) {
    if (key.toLowerCase() === ORDER_KEY) return value.trim();
  }
  return null;
}

/** The one format-aware raw transform for the block-level list property.
 * `splitProps`/`joinProps` are the audited metadata path: they preserve visible
 * body bytes, ignore property lookalikes inside fences, and emit Org drawers.
 * OG writes both the in-memory property and serialized content at
 * `src/main/frontend/modules/outliner/core.cljs:420-433` (6e7afa8eb). */
function rawWithOrderListType(raw: string, value: string | null, format: Format): string {
  const { visible } = splitProps(raw, (key) => key.toLowerCase() === ORDER_KEY, format);
  if (value === null) return visible;
  const property = format === "org" ? `:${ORDER_KEY}: ${value}` : `${ORDER_KEY}:: ${value}`;
  return joinProps(visible, property, format);
}

/** Preserve a source's explicit list type; otherwise inherit the target's.
 * This is OG's common move/insert rule (`outliner/core.cljs:420-433,536-555`
 * at 6e7afa8eb), shared by drag and every structural outline insertion below. */
function rawWithInheritedOrderListType(raw: string, format: Format, targetId: string | null | undefined): string {
  if (orderListTypeFromRaw(raw, format) !== null) return raw;
  const targetType = targetId ? blockProperty(targetId, ORDER_KEY) : null;
  return targetType === null ? raw : rawWithOrderListType(raw, targetType, format);
}

function setOwnNumberedList(id: string, enabled: boolean, visibleText?: string): boolean {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return false;
  const format = formatForBlock(id);
  const base = visibleText === undefined
    ? node.raw
    : joinProps(visibleText, splitProps(node.raw, isBuiltinHidden, format).hidden, format);
  const next = rawWithOrderListType(base, enabled ? "number" : null, format);
  if (next === node.raw) return false;
  pushUndo(`own-numbered:${id}`, [node.page]);
  setDoc("byId", id, "raw", next);
  markDirty(node.page);
  return true;
}

/** Make this block an own numbered-list item. When `visibleText` is supplied,
 * replacing the editor trigger and writing the property are one store mutation. */
export function makeOwnNumberedList(id: string, visibleText?: string): boolean {
  return setOwnNumberedList(id, true, visibleText);
}

export function removeOwnNumberedList(id: string): boolean {
  if (!isOrdered(id)) return false;
  return setOwnNumberedList(id, false);
}

export function toggleOwnNumberedList(id: string): boolean {
  return setOwnNumberedList(id, !isOrdered(id));
}

/** Empty Enter stops only a non-nested own list: an ordered parent keeps the
 * ordinary insert/inherit path. Transcribed from OG
 * `src/main/frontend/handler/editor.cljs:2498-2502` (6e7afa8eb). */
export function stopOwnNumberedListOnEmptyEnter(id: string, visibleText: string): boolean {
  const node = doc.byId[id];
  if (!node || visibleText.trim() !== "" || !isOrdered(id) || isOrdered(node.parent)) return false;
  return removeOwnNumberedList(id);
}
function toLetters(n: number): string {
  let s = "";
  while (n > 0) {
    const r = (n - 1) % 26;
    s = String.fromCharCode(97 + r) + s;
    n = Math.floor((n - 1) / 26);
  }
  return s || "a";
}
function toRoman(n: number): string {
  const map: [number, string][] = [
    [1000, "m"], [900, "cm"], [500, "d"], [400, "cd"], [100, "c"], [90, "xc"],
    [50, "l"], [40, "xl"], [10, "x"], [9, "ix"], [5, "v"], [4, "iv"], [1, "i"],
  ];
  let s = "";
  for (const [v, sym] of map) while (n >= v) { s += sym; n -= v; }
  return s || "i";
}

/** The ordered-list label for a block whose `logseq.order-list-type` is `number`
 *  (else null) — the block's OWN bullet, like OG. The index counts this block
 *  plus the run of consecutive ordered siblings immediately before it; the glyph
 *  cycles number → letter → roman by the depth of consecutive ordered ancestors
 *  (mod 3), so nested ordered lists read 1. → a. → i. like OG. */
export function orderedListMarker(id: string): string | null {
  const node = doc.byId[id];
  if (!node || !isOrdered(id)) return null;
  const siblings = node.parent
    ? doc.byId[node.parent]?.children
    : doc.pages.find((p) => p.name === node.page)?.roots;
  let idx = 1;
  if (siblings) {
    for (let i = siblings.indexOf(id) - 1; i >= 0 && isOrdered(siblings[i]); i--) idx++;
  }
  let depth = 0;
  for (let p = node.parent; isOrdered(p); p = doc.byId[p!]?.parent ?? null) depth++;
  const delta = depth % 3;
  return delta === 0 ? String(idx) : delta === 1 ? toLetters(idx) : toRoman(idx);
}

/** Tick/untick a checkbox on one line of an in-block `+ [ ]` markdown list,
 *  identified by its exact source line. Pure `[ ]`↔`[x]` text swap — round-trips
 *  as standard markdown (and renders/ticks in OG + mobile). */
export function toggleListItem(id: string, rawLine: string) {
  const node = doc.byId[id];
  if (!node) return;
  toggleListItemAtIndex(id, node.raw.split("\n").indexOf(rawLine));
}

/** Flip the `[ ]`/`[x]` checkbox on a SPECIFIC raw line index. Targeting by index
 *  (not line text) is what makes the AST list checkbox toggle safe when two items
 *  share the same label — see toggleAstCheckbox in render/body.tsx. */
export function toggleListItemAtIndex(id: string, lineIndex: number) {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return;
  const lines = node.raw.split("\n");
  const ln = lines[lineIndex];
  if (ln === undefined || !/\[[ xX]\]/.test(ln)) return;
  const next = /\[ \]/.test(ln) ? ln.replace(/\[ \]/, "[x]") : ln.replace(/\[[xX]\]/, "[ ]");
  if (next === ln) return;
  pushUndo(`listcheck:${id}`, [node.page]);
  lines[lineIndex] = next;
  setDoc("byId", id, "raw", lines.join("\n"));
  markDirty(node.page);
}

export type HeadingState = number | true | null;

const MARKDOWN_HEADING = /^#+\s+/;
const clearMarkdownHeading = (raw: string): string => raw.replace(MARKDOWN_HEADING, "");
const setMarkdownHeading = (raw: string, level: number): string => {
  const prefix = `${"#".repeat(level)} `;
  return MARKDOWN_HEADING.test(raw)
    ? raw.replace(MARKDOWN_HEADING, prefix)
    : prefix + raw.trimStart();
};

/** Pure format-aware heading transition shared by single-block and selection
 * commands so their Markdown/Org serialization cannot drift apart. */
function rawWithHeading(raw: string, format: Format, state: HeadingState): string {
  const level = typeof state === "number" && state >= 1 && state <= 6 ? state : null;
  if (format === "org") {
    return orgRawWithProperty(raw, "heading", state === true ? "true" : level === null ? null : String(level));
  }
  if (state === true) return markdownRawWithProperty(clearMarkdownHeading(raw), "heading", "true");
  if (level !== null) return setMarkdownHeading(markdownRawWithProperty(raw, "heading", null), level);
  return markdownRawWithProperty(clearMarkdownHeading(raw), "heading", null);
}

/** Switch between boolean automatic headings and explicit numeric headings.
 * Markdown writes ATX prefixes for numeric state and `heading:: true` for auto;
 * Org writes both states through its property drawer. Each transition clears the
 * incompatible representation. OG parity:
 * `src/main/frontend/handler/editor.cljs:3822-3862` and
 * `src/main/frontend/commands.cljs:623-638` at `6e7afa8eb`; the format-aware
 * property writer is `handler/editor.cljs:888-904` at the same commit. */
export function setHeading(id: string, state: HeadingState) {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return;
  const next = rawWithHeading(node.raw, formatForBlock(id), state);
  if (next === node.raw) return;
  pushUndo(`heading:${id}`, [node.page]);
  setDoc("byId", id, "raw", next);
  markDirty(node.page);
}

/** Apply a context heading command to the active selection, falling back to the
 * pointer block only when no selection is active. The preflight makes a mixed
 * writable/read-only selection an exact no-op. */
export function setSelectionHeading(pointerId: string, state: HeadingState): boolean {
  const selected = selectedIdsImpl!();
  const ids = selected.length ? selected : [pointerId];
  if (!ids.length || ids.some((id) => !blockWritable(id))) return false;

  const changes = ids.map((id) => ({
    id,
    page: doc.byId[id].page,
    raw: rawWithHeading(doc.byId[id].raw, formatForBlock(id), state),
  })).filter((change) => change.raw !== doc.byId[change.id].raw);
  if (!changes.length) return true;

  const pages = [...new Set(changes.map((change) => change.page))];
  pushUndo("heading-selection", pages);
  setDoc(produce((stateDoc) => {
    for (const change of changes) stateDoc.byId[change.id].raw = change.raw;
  }));
  for (const page of pages) markDirty(page);
  return true;
}

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const pad2 = (n: number) => String(n).padStart(2, "0");

/** Read a block's SCHEDULED/DEADLINE date as {y,m,d} (m 0-based), or null. */
/** Normalize an org time token to zero-padded `HH:mm`. mldoc/OG accept an
 *  unpadded hour (`9:05`) and drop seconds; we canonicalize to `09:05` so a
 *  native `<input type="time">` can pre-fill it and the on-disk form matches OG's
 *  rendered (zero-padded) canonical form. Returns null if it isn't `H:mm`. */
function normalizeHHmm(t: string): string | null {
  const m = /^(\d{1,2}):(\d{2})/.exec(t);
  if (!m) return null;
  return `${pad2(+m[1])}:${m[2]}`;
}

export function readSchedule(
  id: string,
  which: "scheduled" | "deadline"
): { y: number; m: number; d: number; time: string | null; repeater: string | null } | null {
  const node = doc.byId[id];
  if (!node) return null;
  const tag = which === "scheduled" ? "SCHEDULED" : "DEADLINE";
  // Capture the optional time (`HH:mm`) and org repeater cookie (`+1w`, `.+1w`,
  // `++1w`) — both after the weekday, in OG's fixed order `<date wday time repeater>`
  // — so re-opening the picker pre-fills the existing time AND recurrence. The
  // weekday is `[A-Za-z]+` (mldoc consumes any letters; OG writes English 3-letter).
  const m = new RegExp(
    `^${tag}:\\s*<(\\d{4})-(\\d{2})-(\\d{2})(?:\\s+[A-Za-z]+)?(?:\\s+(\\d{1,2}:\\d{2}))?(?:\\s+((?:\\.\\+|\\+\\+|\\+)\\d+[dwmy]))?`,
    "m"
  ).exec(node.raw);
  return m
    ? { y: +m[1], m: +m[2] - 1, d: +m[3], time: m[4] ? normalizeHHmm(m[4]) : null, repeater: m[5] ?? null }
    : null;
}

/** Set or clear a block's SCHEDULED/DEADLINE org-timestamp (line 2, like OG).
 *  `repeater` is an org recurrence cookie (`+1w`, `.+1w`, `++1w`) or null; `time`
 *  is a `HH:mm` clock time or null. Both are written inside the `<…>` in OG's fixed
 *  order — `<yyyy-MM-dd EEE[ HH:mm][ repeater]>` — the repeater is consumed by
 *  repeat.ts on completion. */
export function setSchedule(
  id: string,
  which: "scheduled" | "deadline",
  date: { y: number; m: number; d: number } | null,
  repeater?: string | null,
  time?: string | null
) {
  const node = doc.byId[id];
  if (!node || !blockWritable(id)) return;
  pushUndo(`sched:${id}:${which}`, [node.page]);
  const tag = which === "scheduled" ? "SCHEDULED" : "DEADLINE";
  // Remove the old planning line ONLY from the canonical head region (the run of
  // planning/property lines right after the first line) — a `SCHEDULED:` inside a
  // code fence or body text is content and must never be touched (review finding:
  // the old any-line filter deleted fenced planning-lookalikes).
  const all = node.raw.split("\n");
  const isHeadLine = (l: string) => /^\s*(SCHEDULED|DEADLINE):/.test(l) || PROP_LINE.test(l);
  let headEnd = 1;
  while (headEnd < all.length && isHeadLine(all[headEnd])) headEnd++;
  const targetLine = new RegExp(`^\\s*${tag}:`);
  const targetTimestamp = new RegExp(`^\\s*${tag}:\\s*<[^>]+>(.*)$`);
  const keptHead: string[] = [];
  const trailingBody: string[] = [];
  for (const line of all.slice(1, headEnd)) {
    if (!targetLine.test(line)) {
      keptHead.push(line);
      continue;
    }
    const match = targetTimestamp.exec(line);
    if (match?.[1].trim()) trailingBody.push(match[1]);
  }
  // A glued suffix is user body content, not part of the replaced timestamp.
  // Keep the canonical planning/property head contiguous and split that suffix
  // into body lines immediately after it instead of dropping bytes.
  const lines = [all[0], ...keptHead, ...trailingBody, ...all.slice(headEnd)];
  if (date) {
    const wd = WEEKDAYS[new Date(date.y, date.m, date.d).getDay()];
    const hhmm = time ? normalizeHHmm(time) : null;
    const timePart = hhmm ? ` ${hhmm}` : "";
    const rep = repeater ? ` ${repeater}` : "";
    const stamp = `${tag}: <${date.y}-${pad2(date.m + 1)}-${pad2(date.d)} ${wd}${timePart}${rep}>`;
    lines.splice(Math.min(1, lines.length), 0, stamp);
  }
  setDoc("byId", id, "raw", lines.join("\n"));
  markDirty(node.page);
}

/** A block's raw with `collapsed:: true` added or removed so the persisted
 *  property matches the collapsed state. OG stores collapse in the file as a
 *  block property, so mirroring it here makes a collapse survive a relaunch and
 *  show up collapsed in OG / the mobile app. Fence-aware via splitProps. */
function rawWithCollapsed(raw: string, collapsed: boolean, format: Format): string {
  if (format === "org") return orgRawWithProperty(raw, "collapsed", collapsed ? "true" : null);
  const { visible, hidden } = splitProps(raw, isBuiltinHidden, format);
  const nextHidden = upsertPropertyLine(hidden, "collapsed", collapsed ? "true" : null) ?? "";
  return joinProps(visible, nextHidden, format);
}

/** Set a block's collapsed state AND mirror it into its raw `collapsed::` so it
 *  persists — the on-disk markdown is the source of truth on the next load. */
function writeCollapsed(id: string, collapsed: boolean) {
  const n = doc.byId[id];
  if (!n || !blockWritable(id)) return;
  const nextRaw = rawWithCollapsed(n.raw, collapsed, formatForBlock(id));
  setDoc("byId", id, "collapsed", collapsed);
  if (nextRaw !== n.raw) setDoc("byId", id, "raw", nextRaw);
  bumpCollapseEpoch(id);
}

/** Collapse or expand a block and its entire descendant subtree. */
export function setCollapsedDeep(id: string, collapsed: boolean) {
  if (!blockWritable(id)) return;
  pushUndo("collapse-all", [doc.byId[id].page]);
  const walk = (bid: string) => {
    const n = doc.byId[bid];
    if (!n) return;
    if (n.children.length) writeCollapsed(bid, collapsed);
    n.children.forEach(walk);
  };
  walk(id);
  markDirty(doc.byId[id].page);
}

/** Every descendant that can itself be folded, including descendants hidden by
 * a collapsed ancestor. Iterative model traversal avoids both DOM dependence and
 * call-stack growth on a deeply nested outline. The guide's own block is excluded. */
export function collapsibleDescendantIds(id: string): string[] {
  const root = doc.byId[id];
  if (!root) return [];
  const result: string[] = [];
  const stack = [...root.children].reverse();
  while (stack.length) {
    const childId = stack.pop()!;
    const child = doc.byId[childId];
    if (!child) continue;
    if (child.children.length) result.push(childId);
    for (let i = child.children.length - 1; i >= 0; i--) stack.push(child.children[i]);
  }
  return result;
}

/** Persist one collapse value across every collapsible descendant, but never the
 * guide parent itself. One snapshot + one store transaction makes the operation
 * one Undo step and avoids a reactive update per node on large subtrees. */
export function setCollapsedDescendants(id: string, collapsed: boolean) {
  const root = doc.byId[id];
  if (!root || !blockWritable(id)) return;
  const changes = collapsibleDescendantIds(id)
    .map((childId) => {
      const child = doc.byId[childId];
      if (!child || child.collapsed === collapsed) return null;
      return {
        id: childId,
        raw: rawWithCollapsed(child.raw, collapsed, formatForBlock(childId)),
      };
    })
    .filter((change): change is { id: string; raw: string } => change !== null);
  if (!changes.length) return;
  pushUndo("collapse-descendants", [root.page]);
  setDoc(
    produce((state) => {
      for (const change of changes) {
        const child = state.byId[change.id];
        if (!child) continue;
        child.collapsed = collapsed;
        child.raw = change.raw;
      }
    })
  );
  bumpCollapseEpochs(changes.map((change) => change.id));
  markDirty(root.page);
}

export function toggleCollapse(id: string) {
  const n = doc.byId[id];
  if (!n || !blockWritable(id) || n.children.length === 0) return;
  pushUndo("collapse", [n.page]);
  writeCollapsed(id, !n.collapsed);
  markDirty(n.page);
}

/** Expand every collapsed ancestor of `id` so the block itself can render, as
 *  one undo step. Returns true if anything changed.
 *
 *  Needed because a collapsed parent does not render its children into the DOM
 *  at all (`Block.tsx`'s `<Show when={… && !collapsed()}>`), so "navigate to this
 *  block, scroll to it and highlight it" silently does nothing when the target is
 *  hidden — GH #258, reported against Ctrl+Shift+K block results.
 *
 *  The expansion is deliberately persistent, exactly like expanding by hand:
 *  `collapsed::` is on-disk state, and leaving the outline visually expanded but
 *  unsaved would revert under the user on the next load. One `withUndoUnit`
 *  keeps the whole chain a single Ctrl+Z. */
export function expandAncestors(id: string): boolean {
  const target = doc.byId[id];
  if (!target) return false;
  const collapsedAncestors: string[] = [];
  let parent = target.parent;
  while (parent !== null && parent !== undefined) {
    const node = doc.byId[parent];
    if (!node) break;
    if (node.collapsed) collapsedAncestors.push(parent);
    parent = node.parent;
  }
  if (collapsedAncestors.length === 0) return false;
  if (!collapsedAncestors.every((ancestor) => blockWritable(ancestor))) return false;
  withUndoUnit("reveal-block", [target.page], () => {
    for (const ancestor of collapsedAncestors) writeCollapsed(ancestor, false);
  });
  markDirty(target.page);
  return true;
}

/** Explicitly collapse or expand a block (no-op if it has no children or is
 *  already in the requested state). */
export function setCollapsed(id: string, collapsed: boolean) {
  const n = doc.byId[id];
  if (!n || !blockWritable(id) || n.children.length === 0 || n.collapsed === collapsed) return;
  pushUndo("collapse", [n.page]);
  writeCollapsed(id, collapsed);
  markDirty(n.page);
}
export { isOrdered, orderListTypeFromRaw, rawWithCollapsed, rawWithInheritedOrderListType, rawWithOrderListType };
