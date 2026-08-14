# Managed activation authority and validation census

This is the Packet 0 implementation ledger for
[ADR 0054](adr/0054-lazy-genesis-managed-activation.md). It classifies the
current graph-sized work before the production cutover. A row may be deleted
only together with its named replacement or retained validator.

## State and index ownership

| Current state/index | Current role | Target classification | Required action |
|---|---|---|---|
| Exact Markdown/Org path and bytes | Source capture and projection | Durable semantic authority before the final marker; exact recovery/export projection afterwards | Read once into canonical page records; retain one non-parsing final live-tree comparison. |
| Bootstrap operation spool, part manifests, object packs, aggregate and commit | Initial history expressed as interactive operations | Superseded activation encoding | Replace with the lazy-genesis pack and one genesis commit; keep only offline old-format recovery if required. |
| Accepted document and batch maps | Managed causal/document authority | Durable semantic authority | Construct terminal roots once from genesis and continue to update through ordinary accepted operations. |
| External immutable-home/block-claim index | Enforces one durable home for a block | Minimal identity authority plus a rebuildable point index | Preserve the ownership fact; build its point index once from genesis rather than once per imported operation. |
| Portable path index and conflicts | Prevents two physical paths from owning one portable identity | Minimal identity authority | Preserve and construct once from terminal genesis state. |
| Page-name ownership index and conflicts | Selects canonical page-name ownership | Minimal identity authority | Preserve and construct once from terminal genesis state. |
| Persisted Logseq UUID claim index | Selects one live block claim for `id::` identity | Minimal identity authority | Preserve and construct once from terminal genesis state. |
| Reference catalog source-coverage root | Proves which pages were re-extracted | Disposable derived projection | Remove from the new accepted frontier; SQLite generation/frontier stamps cover projection freshness. |
| Reference catalog facts root | Stores page-name, alias and block-reference postings | Disposable derived projection | Lower the canonical page record directly into SQLite; do not publish Patricia nodes. |
| Reference catalog reverse-candidate root | Accelerates backlinks and reference queries | Disposable derived projection | Use the shared SQLite projection. |
| Current-path catalog | Accelerates watcher/reconciliation enumeration | Tine-native operational index derived from accepted semantic effects | Retain only if measurements show it is more appropriate than SQLite; it is not genesis authority. |
| SQLite materialization | Search/query/reference/navigation acceleration | Disposable derived projection | Bulk-load once from canonical page records and rebuild on schema/frontier mismatch. |
| Exact-source shadow manifest | Proves the user-visible projection corresponds to captured source | Recovery/projection evidence | Emit from the same page records; do not reparse or render the page a second time. |
| Migration source backup | Rollback/recovery payload | Durable recovery evidence, not managed semantic authority | Publish from captured exact bytes and carry one same-process construction receipt. |
| Parser/rendered-document caches | Open-editor/render acceleration | Tine-native operational state | Keep bounded and page-local; never use as durable authority or a second semantic index. |

## Graph-sized validation calls

| Current call/phase | Classification | New-format rule |
|---|---|---|
| Initial source semantic admission | Required construction proof | Parse each page exactly once and place the typed result in `ActivationPageRecord`. |
| `spool_bootstrap_operations` reread and parse | Same-process redundant | Deleted with interactive operation expansion. |
| `author_bootstrap_parts` generic transaction preparation/application | Superseded construction mechanism | Deleted from production activation; ordinary post-activation edits still use it. |
| Reference catalog Patricia preparation/publication | Disposable projection built as authority | Deleted from new-format activation and accepted-frontier identity. |
| Prepared publication validation before immutable install | Same-process construction proof | Return a move-only genesis publication receipt. |
| Full aggregate validation inside publication | Same-process redundant | Consume the construction receipt; retain the validator for restart/recovery. |
| `retain_inactive_bootstrap_accepted_authority` full aggregate reload | Same-process redundant | Consume the same receipt; no graph-sized reread. |
| Migration-backup stage/final/final-after-marker tree verification | Two same-process redundant readbacks around one required proof | Writer receipt crosses rename and marker publication; cold recovery still verifies independently. |
| SQLite accepted-engine materialization | Duplicate semantic derivation | Consume canonical page records directly. |
| SQLite reference-catalog traversal | Duplicate derived-fact derivation | Consume the page record's canonical reference facts directly. |
| `materialized_row_digest_for_harness` on a fresh production candidate | Test oracle executed in production | Remove from production; retain only differential/test use. |
| SQLite close, publish, reopen and exact-frontier authentication | Same-process redundant | Keep the writer/candidate open and transfer a one-use receipt; cold open validates schema/frontier/checkpoint. |
| Exact-source shadow planning parse/render pass | Same-process redundant | Consume exact-source evidence from the page record. |
| Final complete live source scan | Final source concurrency proof | Retain exactly once, byte/inventory only, with no parse. |
| Promotion aggregate/event/anchor reloads | Same-process redundant | Widen the promotion receipt to carry the constructed terminal roots and open handles; cold/recovery paths remain independent. |
| Cold open durable-root/checkpoint validation | Cold/recovery proof | Retain. Point payload reads authenticate content; a deep validator remains explicit/recovery-only. |
| Explicit integrity audit and incomplete-recovery validation | Cold/recovery proof | Retain independently of process-only receipts. |

## Reference-catalog removal proof

`ReferenceCatalogRootV2` contains three owned Patricia roots:
`source_coverage_root`, `facts_root`, and `reverse_candidates_root`. Its
`page_name_authority_root` and `external_uuid_claim_authority_root` fields are
only digests binding it to separately owned authorities. The hot engine keeps
page-name ownership in `PageNameOwnershipRootV1` and persisted block UUID
ownership in `LogseqClaimIndexRoot`; operation acceptance consults those
separate indexes directly.

The reference catalog is presently included in `AcceptedFrontierRoot`, its
state digest, and every `AcceptedBatchEvidence`, so current code authenticates
derived reference state as part of local accepted-frontier identity. This is an
encoding choice, not a semantic prerequisite: reference facts are extracted
from accepted page content and SQLite can rebuild them. The new frontier schema
must omit the catalog root and reference delta while retaining the separate
identity roots and making projection freshness depend on the semantic
frontier/generation. Ordinary operation application then emits derived SQLite
deltas after semantic acceptance; failure or staleness rebuilds SQLite rather
than refusing semantic history.

## Baseline and cutover counters

The old path is expected to fail the final ADR gates. At commit `bde9606a`:

- Linux, 1,000 pages / about 10,000 blocks: 82.1 billion activation-only
  instructions, about 1.07 million syscalls, about eight parser calls per page;
- Windows, 13,001 pages / about 130,000 blocks: source capture 21.6 seconds,
  detached authoring 260.4 seconds, of which reference Patricia construction
  was 244.2 seconds, and SQLite was reached only after about 312 seconds;
- the current source structurally contains interactive bootstrap operation
  expansion, accepted-frontier reference-catalog identity, and a production
  full materialized-row digest.

The production switch may occur only when instrumentation and source guards
prove: one initial parse per page, one final non-parsing byte scan, no
interactive operation replay, no eager mutable document for an untouched
genesis page, no reference-posting Patricia nodes in the new format, no
production full-row digest, and no same-process full revalidation after a
consumed construction receipt.
