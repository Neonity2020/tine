/* tslint:disable */
/* eslint-disable */

export function logbook_apply_marker_transition(raw: string, is_org: boolean, old_marker: string, new_marker: string, enabled: boolean, with_seconds: boolean): string;

export function logbook_clock_in(raw: string, is_org: boolean, with_seconds: boolean): string;

export function logbook_clock_out(raw: string, with_seconds: boolean): string;

export function logbook_info_json(raw: string): string;

/**
 * The lsdoc git tag this wasm was built against (set by `build:wasm` via the
 * `LSDOC_TAG` env, read from tine-core's Cargo.toml — the single source of truth).
 * Surfaced to the frontend for diagnostics; the hard stale-wasm guard lives in the
 * build:wasm script (it refuses to build if this crate's pin ≠ tine-core's pin).
 * See docs/wasm-parse-plan.md §7D.
 */
export function lsdoc_tag(): string;

/**
 * Parse one de-bulleted block body into lsdoc's render AST, serialized to JSON.
 *
 * Mirrors `tine_core::render::parse_block` exactly. Both bridges compile the same
 * shared boundary helper: OG-compatible re-bullet parsing plus Tine's deliberate
 * correction for line-leading Markdown inline code containing `::`.
 */
export function parse_block_json(raw: string, is_org: boolean): string;

/**
 * Parse a WHOLE FILE (raw graph file text, NOT re-bulleted) into lsdoc's observable
 * projection `{blocks, refs}`, serialized to JSON — the same thing the `lsdoc-parse`
 * CLI emits. Unlike `parse_block_json` (one de-bulleted block), this is document-level,
 * for the "Help improve Tine" diff panel, which compares whole files against mldoc
 * exactly as `lsdoc/tools/graph-check.mjs` does. Not on the render path.
 */
export function parse_document_json(text: string, is_org: boolean): string;

/**
 * Render one de-bulleted block body to lsdoc's CANONICAL HTML skeleton (M3 render
 * contract — `lsdoc::render_html`): structural tags + classes + `data-*` hooks, no
 * ref/asset/math/macro resolution. Re-bullets EXACTLY like `parse_block_json` so the
 * rendered AST is identical, then renders it.
 *
 * NOT on the app's render path — the frontend renders the AST reactively (interactive
 * DOM, resolved refs/assets), never lsdoc's HTML string. This exists ONLY so the
 * anti-drift gate (`src/render/skeleton-drift.test.tsx`) can compare lsdoc's canonical
 * skeleton against the frontend's reactive skeleton, from the SAME wasm the app ships —
 * catching drift between the two renderers (Option C2: both conform to one skeleton).
 */
export function render_block_html(raw: string, is_org: boolean): string;

export function __tineReinstantiate(): void;

/**
 * Fold text with Tine's exact A6 search transform.
 *
 * This is whole-string lowercase plus compatibility decomposition, removal of
 * Unicode nonspacing marks, canonical reorder and composition. It deliberately
 * is not full Unicode casefold and is not idempotent for every compatibility
 * character, so callers must pass raw text and invoke it exactly once.
 */
export function search_fold(text: string): string;

/**
 * Fold raw text once and serialize its provenance as
 * `{text, sources: [{start, end}]}`. Each source range uses raw UTF-16 offsets,
 * and there is exactly one range per output Unicode scalar (not per UTF-16 code
 * unit).
 */
export function search_fold_map_json(text: string): string;

/**
 * Parse one shared search matcher and apply it to a JSON string array.
 *
 * Boolean queries fold each raw candidate exactly once; regex queries see raw
 * text only. Empty and invalid-regex queries retain every candidate, with the
 * latter returning its diagnostic in `search_error`. Malformed JSON is a bridge
 * error rather than a fabricated match-all result.
 */
export function search_match_batch_json(query: string, texts_json: string): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly logbook_apply_marker_transition: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number) => [number, number];
    readonly logbook_clock_in: (a: number, b: number, c: number, d: number) => [number, number];
    readonly logbook_clock_out: (a: number, b: number, c: number) => [number, number];
    readonly logbook_info_json: (a: number, b: number) => [number, number];
    readonly lsdoc_tag: () => [number, number];
    readonly parse_block_json: (a: number, b: number, c: number) => [number, number];
    readonly parse_document_json: (a: number, b: number, c: number) => [number, number];
    readonly render_block_html: (a: number, b: number, c: number) => [number, number];
    readonly search_fold: (a: number, b: number) => [number, number];
    readonly search_fold_map_json: (a: number, b: number) => [number, number];
    readonly search_match_batch_json: (a: number, b: number, c: number, d: number) => [number, number, number, number];
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
