# 0009. One source for block-header facets: the lsdoc parse, shipped on the DTO

- **Status:** Accepted
- **Date:** 2026-06-30

## Context

A block's *header facets* — its task marker, `[#A]` priority, heading level,
`key:: value` properties, and `SCHEDULED:`/`DEADLINE:` planning dates — were derived
in **three** places, each with its own hand-rolled scanner: the TS `blockView` regex
loop ([src/render/block.ts]), the Rust `doc.rs`/`query.rs` line scans, and `lsdoc`
(which already parses every block for inline refs and computes all of these natively).
Three parsers of the same grammar can disagree, and one did: `blockView` matched
`SCHEDULED:` anywhere on a line, so a `` `DEADLINE: <…>` `` written inside inline code
was wrongly stripped and shown as a date badge. The render path also fed lsdoc a
*header-stripped* body, so the parser it had couldn't even help.

Making lsdoc the single source has a tension with [ADR 0008](0008-lazy-block-body-rendering.md):
the `Block` component (and so its marker chip) stays mounted for **every** block,
including off-screen ones, while only the *body* render is deferred. Deriving facets
from a frontend lsdoc parse would therefore re-parse every block on load — partially
undoing 0008's win. The alternatives were: (a) frontend parses each block for its
facets (simplest, but the load-parse regression), or (b) compute facets once in Rust
and ship them.

## Decision

We will treat the **one lsdoc parse as the single source** for every block-header
facet, and **(b)** carry the facets across the IPC boundary on `BlockDto`.

- Rust `DocBlock::projection()` computes marker / heading / properties / visible-text /
  scheduled / deadline off its single lsdoc parse; `block_to_dto` ships them on
  `BlockDto` (priority stays the existing `[#A]` char recognizer — already singular).
- The frontend seeds a raw-keyed **facet cache** (`src/render/facets.ts`) from the DTO
  at load — **zero frontend parse on load** (0008 intact). The block being *edited* is
  the only cache miss; it derives from one in-browser wasm parse.
- `AstBody` renders the body from the same whole-block parse, skipping the property and
  planning nodes that the chrome draws. `blockView` is **deleted**; its body-text role
  survives as `visibleBody` (labels + reference panel) — text only, no facet derivation.
- `SCHEDULED`/`DEADLINE` are recognized only when lsdoc emits a real `Timestamp`, and on
  block exit a real planning line is normalized to its canonical position (after the
  first line, before properties — OG layout).

## Consequences

- A block's chip, its query/search/carry behavior, and its rendered body can no longer
  disagree — they read one parse. The inline-code planning-badge bug is fixed by
  construction, in every surface.
- `BlockDto` grows six optional, serde-skipped facet fields — a small wire cost, and a
  contract: the backend owns these, the frontend reads them (and recomputes locally only
  while editing). Round-trip is unaffected (facets are derived, `raw` stays authoritative).
- Off-render-path facet needs (carry's `isOpenTask`) use the parser-free markers.ts
  recognizer (same vocabulary as lsdoc → no disagreement), so they don't pull in the
  wasm renderer.
- Commits us to keeping lsdoc's `marker`/`Properties`/`Timestamp`/`span` fields stable
  in the wire format (the FOR-TINE contract). Follow-on: page-level facets and the
  export (`publish.rs`) plain-text path are not yet unified — a later step.
