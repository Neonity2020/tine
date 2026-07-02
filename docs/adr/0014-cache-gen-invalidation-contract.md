# 0014. Derived caches self-invalidate via `cache_gen` — no hand-rolled invalidation

Date: 2026-07-02

## Status

Accepted

## Context

`tine-core` keeps many derived structures (alias map, block index, ref counts,
page list, memoized query results, …). Per-entry invalidation logic is the
classic way such caches rot. `model.rs` stays coherent at 4000+ lines largely
because it never does that.

## Decision

- Every derived cache in `Graph` is keyed by the single **`cache_gen`** atomic,
  bumped on *any* cache mutation. A cached value whose recorded gen doesn't
  match is dead — there is no per-entry invalidation to get wrong.
- Memoized whole-graph derivations go through **`derived_memo`**; precise
  post-save eviction exists in exactly one place, `scope_derived_invalidation`.
  If a result can't be scoped safely, the correct fallback is full invalidation
  (a perf cost), never staleness (a correctness cost).
- Lock order is **`page_lock` → cache → `disk_revs`** — never acquire in
  another order.
- New derived data (indices, counts, projections) must use this pattern.
  A bulk read over pages goes through `with_pages` once — never a loop of
  per-name `load_named`/`find_entry` calls (each is a directory scan).

## Consequences

- "Optimizing" a cache by keeping it alive across mutations without a
  `cache_gen` key is how staleness bugs would enter; this ADR exists so a
  well-meaning future change doesn't do that.
- The pattern is deliberately coarse; `derived_cache_fuzz.rs` differentially
  tests it and must keep passing.
