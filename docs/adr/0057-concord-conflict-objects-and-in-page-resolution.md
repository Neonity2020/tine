# 0057. A conflict is a derived, multi-sided object resolved in the page

- **Status:** Accepted
- **Date:** 2026-08-17

## Context

Concord (GH #337, spec L3–L5) had, before this, three separate conflict
surfaces and no inventory:

- sync-tool conflict copies were listed in Settings → Backups & recovery, with
  a modal two-column merge dialog (ADR 0020, upgraded to 3-way suggestions by
  ADR 0056);
- VCS-marker-bearing pages were listed in the same Settings area and carried a
  page banner, and were **only** quarantined — Tine refused to write them and
  told the user to go and finish the merge in another tool;
- the two shared no data model, so nothing could say how many things needed
  the user's judgement, and nothing survived a restart except by re-walking the
  two listings independently.

Three design pressures shaped what replaces it:

1. **Invariant 1 forbids inventing state.** Files stay plain valid Logseq
   Markdown; Concord writes no markers and no metadata of its own. So a
   conflict cannot be a stored record inside the graph, and any store outside
   it would be a second source of truth to keep coherent with disk.
2. **Two sides is the wrong arity.** A `diff3`/Fossil marker block carries
   *three* versions (ours, common ancestor, theirs), and ADR 0056's ledger
   supplies a third side for conflict copies. A pair type would have had to be
   widened later, in a data model many surfaces read.
3. **The Settings modal is the wrong place.** EllisMorrow's GH #337 mockup, the
   VS Code merge editor and `smerge` all put the versions *where the content
   is*. A modal in a settings screen is also, structurally, an interruption —
   the opposite of jj's calm persistent conflict object.

## Decision

**1. A conflict object is derived, never stored.**
`Graph::conflict_queue()` computes the whole inventory from disk on every call,
from the two existing recognizers (`list_sync_conflicts`,
`list_vcs_marker_conflicts`). There is no cache, no index, and nothing written
to the graph. The queue therefore "survives restarts" in the only way that
cannot go stale: it is recomputed, and the same on-disk state yields the same
objects with the same derived ids (`copy:<path>` / `markers:<path>`). Block
counts are computed inline because conflicts are few and each costs one parse
of two small texts — the two directory walks the recognizers already do
dominate.

**2. Sides are a list, not a pair.** `ConflictObject.sides: Vec<ConflictSide>`
with roles `mine` / `theirs` / `base`. A base side appears whenever one is
actually known — the ledger's, or the ancestor a `diff3`/Fossil marker block
carries with it — and its absence is expressed by the list, not by an
`Option<T>` bolted onto a pair.

**3. Resolution happens at the page, through the existing guarded write.**
`PageConflictResolution` renders the object for the page in view, above the
outline: sides named by whatever produced them, coloured consistently, per-block
keep-mine/keep-theirs/keep-both, an `n conflicts ↑↓` walk, and a batch
"apply all suggested". Nothing auto-applies. Applying calls the unchanged
`resolve_sync_conflict` for a copy, and the new `resolve_vcs_marker_conflict`
for a marker page — which reuses the same page lock, `base_rev` staleness
guard, org round-trip firewall, `merge_blocks` alignment and recoverability
guarantees.

**4. The opening position is the suggested resolution, and keep-both where
there is none.** Where a base answers the question, that side arrives
pre-selected and labeled *suggested* (ADR 0056's mechanism, unchanged). Where
it does not — both sides changed, or no ancestor is known — the pre-selection
is keep-both, which loses nothing and writes the two versions as **adjacent
sibling blocks**: ordinary outline Markdown, readable by every other tool, with
no marker or property recording that the block was contested.

**5. Markers are parsed, never invented (L5).**
`concord_queue::parse_vcs_marker_sides` reconstructs two or three COMPLETE page
texts from the marker sections and feeds them to the same `sync_diff`
machinery, so the marker path and the copy path are one renderer and one merge
engine. Marker recognition is delegated to `doc::scan_vcs_conflict_markers` —
the identical scanner the save refusal uses — so the code that refuses to
rewrite a conflicted file and the code that resolves one cannot disagree about
what a marker is. A base is offered only if *every* region supplied one; a
malformed marker structure yields `None` and the file is left strictly alone.

**6. Invariant 3 keeps exactly one exemption, scoped to one path and one
write.** `Graph::marker_resolutions` holds the path currently inside an
authorized resolution, taken under that page's lock and released by an RAII
guard (including on early return or panic). `serialize_page_document` consults
it, so a concurrent editor save to any *other* marker-bearing page is still
refused. Once the resolution lands the file has no markers, so the quarantine
lifts by itself — no state has to be told about it.

**7. One renderer.** `DiffRowView` and the decision helpers moved to
`components/DiffRows.tsx`; the Settings modal and the in-page resolver both
import them, differing only by props (column wording, the initial-decision
policy). This is deliberate: two independently-written renderers over the same
data drifted apart silently once already in this codebase (the two block-facet
renderers).

## Relation to ADRs 0012, 0020 and 0056

- **0020 (conflict-copy merge)** is not replaced. Its alignment, row ids,
  `merge_blocks` semantics, resolve guards and stage-before-commit are used
  verbatim by both paths, and its Settings modal remains as the fallback
  surface. What changes is *where* the review is offered and that a second
  artifact source now reaches the same engine.
- **0056 (base ledger, 3-way)** is not changed. Its suggestions gain a second
  producer — a marker block's own `|||||||` ancestor, which needs no ledger at
  all — and a second consumer, the in-page resolver.
- **0012 (recoverable trash / never delete)** still governs the copy path: a
  resolved copy is trashed, not deleted. The marker path removes nothing from
  disk; it rewrites one file in place, and the pre-resolution bytes remain
  reachable through the ordinary backup/recovery machinery.

## Consequences

- The user finally has one answer to "what needs me?" — a `N conflicts` badge
  in the sidebar footer — and it costs no storage and cannot desynchronize from
  disk.
- Marker-bearing pages stop being a dead end. Tine is now, as far as we know,
  the only outliner that resolves a `git merge` conflict block-by-block in the
  page and writes back clean Markdown.
- The queue's cost is two directory walks plus a parse per conflict, per
  refresh (a `conflicts-changed` event or a Settings visit). That is acceptable
  only while conflicts are few; if a graph could hold hundreds, this becomes a
  cache decision — and the ADR's derivation stance means the cache would have to
  be disposable and app-private, like the ledger.
- Fossil's `####### SUGGESTED CONFLICT RESOLUTION` section is parsed and
  deliberately excluded from every reconstructed side: it is a derivation, not a
  version. Adopting it as a fourth, pre-selectable proposal is left open.
- The write exemption is one path in one mutex. It is the only way any Tine
  code can write a marker-bearing file, and it is enforced by
  `marker_resolution_is_guarded_and_never_leaves_the_file_writable`.
