# 0015. The lsdoc wire contract: what Tine may assume about lsdoc's output

Date: 2026-07-02

## Status

Accepted

## Context

lsdoc is maintained as a separate project (ADR 0005/0006) and is under active
redesign. Version pinning is strong (Cargo tags + vendored-WASM tag checked by
`scripts/check-wasm-pin.mjs` + a boot assert), but a pin can't catch a *shape*
change: the frontend consumes lsdoc JSON via an unchecked
`JSON.parse(...) as Block[]` against the hand-maintained mirror
`src/render/ast.ts`. A renamed/retyped field would read as `undefined` silently.

## Decision

The Tine-facing surface of lsdoc — "FOR-TINE" — is a contract:

- **AST fields Tine reads** (via `src/render/ast.ts` and `BlockDto` facets:
  markers, heading, properties, planning timestamps, inline `span`s, table
  structure, …) and the **`data-*` hooks** in `render_html` (ADR 0010) are
  stable names. lsdoc never resolves live concerns (refs, assets, macros) —
  it emits hooks; Tine resolves.
- A lsdoc bump that changes any of these must, in the same Tine change: update
  `ast.ts`, the skeleton-drift gate fixtures, and the AST contract test.
- Contract gates, in order of what they catch: the triple version pin (stale
  artifacts), the boot assert (mismatched wasm at runtime), skeleton-drift in
  CI (render divergence), and an **AST contract test** (field-shape drift —
  the gap this ADR closes; a fixture of raw inputs whose parsed JSON must
  contain the exact keys `ast.ts` depends on).

## Consequences

- With the contract test in place, Tine and lsdoc can safely evolve on separate
  schedules — the audit's explicit verdict was to keep them separate.
- Adding a *new* lsdoc feature Tine consumes means adding it to the contract
  fixtures, not just using it.
