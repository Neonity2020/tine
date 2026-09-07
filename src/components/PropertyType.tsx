import { For, Show, createSignal, createUniqueId, type Accessor, type JSX } from "solid-js";
import { ensurePagePropertyOnKeyPage } from "../store";
import type { Cardinality, ObservedType, RegistryRow } from "../editor/queryIr";

/** The five declarable types (§6.3). Not a widening point: the engine's
 *  `parse_declaration` accepts exactly these, with an optional `list of `. */
export const DECLARABLE_TYPES: ObservedType[] = ["text", "number", "date", "checkbox", "ref"];

/** The page property a declaration lives in (`registry.rs::DECLARED_TYPE_KEY`).
 *  Namespaced because bare `type::` is the user's own vocabulary (§6.3). */
export const DECLARED_TYPE_KEY = "tine.type";

/**
 * **Find the registry row a picker key names — without writing a key
 * normalizer (D-14).**
 *
 * The engine's `property_key_norm` is the ONE normalizer, and it does more than
 * lowercase: it also folds spaces and underscores to `-`, so a key authored
 * `due date` has the row `due-date`. Re-implementing that here would be a second
 * producer of a question Rust already answers, and the two would drift on
 * exactly the keys that matter.
 *
 * So this does not normalize: it MATCHES. The picker's keys come from
 * `query_facets`, the registry's rows carry their own `normalized_name`, and an
 * exact hit is the answer. The one relaxation is a display-only fallback for the
 * common shapes (case, surrounding space, ` `/`_` in place of `-`); when even
 * that finds nothing the caller says "no observed values" and shows no type,
 * which is honest. Every WRITE uses the matched row's `normalized_name`
 * verbatim, never a string this module derived.
 */
export function registryRowFor(
  rows: RegistryRow[] | undefined,
  key: string,
): RegistryRow | undefined {
  if (!rows?.length) return undefined;
  const exact = rows.find((row) => row.normalized_name === key);
  if (exact) return exact;
  const relaxed = key.trim().toLowerCase().replace(/[ _]/g, "-");
  return rows.find((row) => row.normalized_name === relaxed);
}

/** `tine.type:: number` / `tine.type:: list of number` (§6.3). */
export function declarationValue(type: ObservedType, cardinality: Cardinality): string {
  return cardinality === "many" ? `list of ${type}` : type;
}

/** What the engine will coerce by: the declaration when there is one, else the
 *  observed majority (`Registry::effective_type`). */
export function effectiveTypeOf(row: RegistryRow): { type: ObservedType; cardinality: Cardinality } {
  const declared = row.declared;
  return declared
    ? { type: declared[0], cardinality: declared[1] }
    : { type: row.observed_type, cardinality: row.cardinality };
}

const typePhrase = (type: ObservedType, cardinality: Cardinality) =>
  cardinality === "many" ? `list of ${type}` : type;

/**
 * **The property registry, made visible and writable by a person (SPEC §6.3,
 * §9 P2).**
 *
 * The engine has computed all of this since P0 — observed type, cardinality,
 * `mismatch_count`, and the declared override read off the key's own page — and
 * nothing in the UI ever looked. This is the whole surface: what a key's type
 * IS, whether anything disagrees with it, and the one action that changes it.
 *
 * Two decisions worth stating, because both are easy to get subtly wrong:
 *
 *  - **A mismatch count is only shown against a DECLARED type.** The registry
 *    computes `mismatch_count` against the *effective* type, which is the
 *    declared one when a declaration exists and the observed majority
 *    otherwise. "3 blocks don't match the majority" is not a defect — it is
 *    what an untyped graph looks like — so showing it would be noise the user
 *    cannot act on. Against a type the user themselves declared it is exactly
 *    the actionable thing.
 *  - **The declaration is written on the row's `normalized_name`, verbatim.**
 *    The engine binds a declaration by `refs::page_key(normalized_name)`, so a
 *    key authored `due date` must be declared on the page `due-date`; a page
 *    literally named `due date` would never bind and the user would be left
 *    with a declaration that does nothing.
 */
export function PropertyType(props: {
  /** The key as the picker has it (a `query_facets` key). */
  propertyKey: string;
  /** The registry rows, or undefined while the one read is in flight. */
  rows: Accessor<RegistryRow[] | undefined>;
  /** Ask the host for a fresh read. Called ONLY after a declaration lands. */
  onDeclarationWritten: () => void;
  /** Hide the "declare type…" action (a read-only surface). */
  readOnly?: boolean;
}): JSX.Element {
  const row = () => registryRowFor(props.rows(), props.propertyKey);
  const [open, setOpen] = createSignal(false);
  const [many, setMany] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const listOfId = `qb-declare-list-of-${createUniqueId()}`;

  const declared = () => row()?.declared;
  const badge = () => {
    const current = row();
    if (!current) return null;
    const d = current.declared;
    return d
      ? { text: typePhrase(d[0], d[1]), source: "declared" as const }
      : { text: typePhrase(current.observed_type, current.cardinality), source: "observed" as const };
  };
  const mismatch = () => {
    const current = row();
    const d = current?.declared;
    // Only against a declaration (see the class doc). `mismatch_count` counts
    // OWNERS — blocks and pages alike — not atoms.
    return d && current!.mismatch_count > 0
      ? `${current!.mismatch_count} ${current!.mismatch_count === 1 ? "block or page doesn't" : "blocks or pages don't"} match ${typePhrase(d[0], d[1])}`
      : null;
  };

  const declare = async (value: string | null) => {
    const current = row();
    if (!current || busy()) return;
    setBusy(true);
    try {
      await ensurePagePropertyOnKeyPage(current.normalized_name, DECLARED_TYPE_KEY, value);
    } finally {
      setBusy(false);
      setOpen(false);
      // The row the badge reads is now stale. This is one of exactly two moments
      // the registry is re-read (§6.4, I-13); nothing else in P2 refetches it.
      props.onDeclarationWritten();
    }
  };

  return (
    <div class="qb-prop-type" onClick={(e) => e.stopPropagation()}>
      <Show
        when={badge()}
        fallback={
          <Show when={props.rows()}>
            <span class="qb-prop-type-none">no observed values</span>
          </Show>
        }
      >
        {(shown) => (
          <span class="qb-prop-type-badge" classList={{ "is-declared": shown().source === "declared" }}>
            {shown().text}
            <span class="qb-prop-type-source"> ({shown().source})</span>
          </span>
        )}
      </Show>
      <Show when={mismatch()}>
        {(line) => <div class="qb-prop-type-mismatch">{line()}</div>}
      </Show>
      <Show when={!props.readOnly && row()}>
        <button
          class="qb-prop-type-declare"
          disabled={busy()}
          onClick={() => setOpen(!open())}
        >
          declare type…
        </button>
        <Show when={open()}>
          <div class="qb-prop-type-menu">
            <label class="qb-prop-type-listof" for={listOfId}>
              <input
                id={listOfId}
                type="checkbox"
                checked={many()}
                onChange={(e) => setMany(e.currentTarget.checked)}
              />
              list of
            </label>
            <For each={DECLARABLE_TYPES}>
              {(type) => (
                <button
                  class="qb-menu-item qb-prop-type-option"
                  onClick={() => void declare(declarationValue(type, many() ? "many" : "one"))}
                >
                  {typePhrase(type, many() ? "many" : "one")}
                </button>
              )}
            </For>
            <Show when={declared()}>
              <button
                class="qb-menu-item qb-prop-type-remove"
                onClick={() => void declare(null)}
              >
                remove declaration
              </button>
            </Show>
          </div>
        </Show>
      </Show>
    </div>
  );
}
