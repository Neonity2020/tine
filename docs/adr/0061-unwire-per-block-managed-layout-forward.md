# 0061. Un-wire the per-block Managed Storage layout forward

- **Status:** Accepted; release gated on the same-window write benchmark
- **Date:** 2026-09-10

## Context

Managed Storage's live write path briefly addressed one Loro document per block
and one per block/page membership (the "P4" layout, introduced by `d7c57e2b`).
Measured against the preceding control, a foreground move of 200 blocks went
from about 30 ms to about 176 ms and its settlement from about 111 ms to about
1.1 s, because such a move changed roughly 601 CRDT documents and archive
objects instead of 2. Repeated edits and subtree deletes slowed down too.

The control's model (`3d21bcaa`) was one catalog document plus one shard
document per page, with each block's content homed in the shard of the page it
was created on. Since then, the integration base has gained foundation work that
must survive: persistent writer lanes, the cold object store, absence decisions
and sweeps, sealed checkpoint generation, the portable path index, the
`tine-storage` v0.20.0 API, the query campaign, and a set of correctness fixes
that landed alongside the per-block layout. Reverting commits or whole files
would lose that work; keeping the per-block layout keeps the write cost.

## Decision

We will un-wire the per-block layout forward, restoring the control's document
model on top of the current code rather than reverting history:

- **Layout.** One catalog document holds page states; one shard document per
  page holds `owners`, `members`, `content` (mergeable text per block), Logseq
  UUIDs and the preamble. A block's immutable home is its creation page's
  shard. A move rewrites owners and source/destination membership; content
  stays home. Live documents are keyed by their 16 UUID bytes everywhere.
- **Retained.** Every foundation listed above, the `retirable_document` and
  `sealed_document_map` codecs as dormant qualification machinery with no live
  producer, `BlockBirth` provenance derived from the creation shard, and every
  correctness fix whose behaviour does not depend on the layout.
- **One current format.** Operation schema 10, semantic-effect schema 8, and
  lazy-genesis manifest and page-capsule schemas 6. A store written by the
  per-block layout is recognized by its containing format and refused with
  `MS-REF-PROTOCOL-INCOMPATIBLE`. The existing graph-open blank-slate lifecycle
  then preserves the whole private root as a backup and rebuilds Managed
  Storage from Markdown/Org automatically. There is no reader, migration or
  compatibility fixture for the old format.
- **Not in scope.** No change to deletion semantics: owner tombstones and
  mergeable text behave as in the control. No shallow checkpoint installation,
  generation cutover, new layout, occupancy caps, Direct Files change or query
  change.

## Consequences

- Write cost returns to the page-shard model. A subtree born on its source page
  moves by changing two documents. A page that has received blocks from other
  pages still references those blocks' original home shards.
- Private per-block stores from test builds are preserved but not replayed.
  Edits that existed only in their journals remain in the backup, not in the
  rebuilt store.
- The dormant codecs and the sealed document roster stay compiled for their
  tests. A later packet may delete them or wire them to a qualified generation;
  this decision does neither.
- The same-window benchmark against a freshly measured control, within 1.2× on
  every row, remains the release gate. The native rebuild journey starts in
  burn-in.
