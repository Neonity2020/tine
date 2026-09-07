# UI state lifetimes

Every UI value belongs to one lifetime. A change that persists or resets a
value must name its owner and lifetime before wiring storage. This prevents a
runtime handle from accidentally becoming durable state and prevents a graph
transition from saving state under the wrong graph.

## Lifetimes

- `device-preference`: owned by the installation, independent of any graph;
  reset by an explicit preference change or application-data reset.
- `graph-configuration`: derived from graph files/configuration and reloaded on
  graph bind or configuration refresh; it is not session state.
- `graph-session`: user view state belonging to one graph or named workspace;
  reset or restored at graph/workspace transitions and persisted through the
  audited session boundary.
- `transient-runtime`: generations, native handles, in-flight work, focus and
  navigation intents; reminted or cleared at the owning runtime boundary and
  never serialized.

## Route-owned graph-session state

Pane snapshots and their route histories are the durable authority for visible
content. `src/session.ts` parses and serializes each route explicitly instead of
spreading runtime objects into storage. A PDF is therefore an ordinary `pdf`
route in a tab, not a second top-level pane state:

| State | Owner | Lifetime | Reset trigger | Persisted representation |
|---|---|---|---|---|
| PDF tab | pane router and session serializer | `graph-session` | tab close, graph switch, or workspace switch | `kind`, stable `viewId`, `filename`, `label`, optional page and scale |

The former top-level `pdfTarget` is accepted only as legacy input. Desktop
restore migrates it into a companion pane in the layout tree; mobile restore
appends it to the active pane history. New sessions never write `pdfTarget` or
a global PDF-pane width.

PDF graph ownership and its generation, pending page/highlight navigation
intents, viewer/native handles, render tasks, and sidecar view state are
`transient-runtime`. They are scoped to the route's `viewId`, reminted or
cancelled at their owning boundary, and never serialized. On restore, Tine uses
the stable resource identity and mints ownership from the current graph bind.

The typed registry in `src/uiStateRegistry.ts` remains the authority for
standalone graph-session signals. It is intentionally empty now that PDF state
lives in pane routes. Adding another standalone signal requires an explicit row
and typed registry decision in the same change.

## The query sheet's own state

Everything the query builder holds while a sheet is open is `transient-runtime`.
None of it is serialized, and all of it is cleared at the boundary that owns it:

| State | Owner | Lifetime | Reset trigger | Persisted representation |
|---|---|---|---|---|
| Query text draft, its parse revision and its last good parse | `QueryTextPane` in `src/components/QueryBuilder.tsx` | `transient-runtime` | any keystroke (which bumps the revision), a replacement session, or unmount | none — the query's own text is PRINTED BY RUST from the saved IR |
| The registry snapshot behind the vocabulary picker | the registry `createResource` in `src/components/QueryBuilder.tsx`, through `sharedQueryResult` | `transient-runtime` | a `dataRev` bump, a declaration written anywhere, or a graph-scope change | none — see `docs/contracts/frontend-staleness.md` Item 3 for the bound |
| Whether that snapshot has LANDED for the current scope and revision (`RegistryAccess.pending`) | derived in `src/components/QueryBuilder.tsx` from the same scope+key gate that exposes the rows | `transient-runtime` | the matching reply settling, or the sheet closing (no sheet open ⇒ no read, so not pending) | none |
| Which popover is open, and the transient dismissal layer | `QuerySheet` / `transientLayers` | `transient-runtime` | Escape, an outside press, or sheet close | none |
| The §7.5 crossing notice, its "don't show again" tick and its one-shot focus | `Macro.tsx` (it is the host that moves the notice between the pane and the block) | `transient-runtime` per notice; the DISMISSAL is `device-preference`, graph-keyed | Undo, Keep it, or a new save | only the dismissal, beside the other window state, never in the graph (I-18, D-11) |

The draft's reset trigger is the point of it. A parse answer — success OR
rejection — is rendered only when it is about the exact revision on screen in a
pane that still exists (I-20), so closing the sheet, switching host block or
typing one more character invalidates every outstanding response rather than
letting a late one describe text nobody is looking at.

The registry row above has a second reading, and both of them are `undefined`
rows: *nobody asked* (no sheet is open, so I-13 means no read happened) and *the
answer for this scope and this declaration revision has not arrived yet*.
`pending` separates them, from the same predicate that gates the rows, because
the surfaces that read them must behave differently: the vocabulary picker shows
a compact loading line instead of a graph with no properties, and a property
row's operator, value and commit wait rather than fall back to the untyped
`text` family (§6.3) — which is exactly the wrong answer in the moment right
after a `tine.type` has been written.

Anchor previews also require the exact current request revision. Selecting the
original anchor, replacing the session, closing the sheet or unmounting cancels
pending success and error responses. The shared key is not a settled watermark.
