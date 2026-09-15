# Storage contract (Direct Files)

This document is the implementation contract for how Tine stores a graph.
Direct Files is Tine's only storage mode: the user's Markdown/Org tree is the
sole authority for page and journal content, and everything Tine keeps beside
it is disposable app-private derived state or recovery material.

Managed Storage, the opt-in oplog runtime this document used to specify, was
removed from Tine on 2026-09-15. Any future synchronization design will be
specified fresh, under its own contract, rather than by reviving the removed
sections. A graph folder that still holds a `.tine-sync/` directory from that
era opens as an ordinary Direct Files graph: the directory is outside graph
text, and Direct publication leaves its bytes untouched
(`ordinary_direct_publish_treats_v1_and_return_recovery_as_inert`).

Section numbers are stable because code and tests cite them (§2.10a-i, §2.10b,
§2.10d, §3 invariant 9). Numbers missing from the sequence belonged to the
removed storage mode.

## 1. On-disk layout

### 1.2 Device-private app data

Tine's per-graph app-private state is disposable derived state or recovery
material; none of it is graph authority. This section covers conflict capsules.
§1.2a covers plugin packages, §1.3 the query projection, and §4 the Concord
base ledger.

Live-save conflicts use an app-private protocol:
`<app-data>/conflict-capsules/<graph-key>.v1.json`,
where `graph-key` is the session-style sanitized graph basename plus FNV-1a of
the root path. `ConflictCapsuleEnvelope` contains the exact retained PageDto,
its load baseline, and page binding.
The whole graph envelope is replaced through `atomic_write` (unique create-new
temporary file, file barrier, atomic rename, directory barrier); stale torn
temporaries are ignored and reclaimed on reopen. An envelope that does not
decode (torn or foreign bytes at this app-private boundary) is set aside as
`<graph-key>.v1.json.unreadable-<uuid>` with a directory barrier and the queue
reopens empty; it never blocks capture or resolution. Explicit resolution re-proves
the graph's authority and durably rewrites the envelope, or removes
and directory-syncs the final file, before the frontend acknowledges success.
For a recovered Direct Files draft, the stored disk revision selects the durable
recovery path but does not authorize the next write. Review returns the revision
of the current disk snapshot it actually displays. Apply rechecks that revision
under the page lock; a subsequent external change returns `conflict.base_rev`
and preserves the newer bytes for another review. The native and browser review
adapters share this rule, covered by
`rehydrates a durable live conflict and applies the currently reviewed disk revision`
and `concord_live_save_conflict_capsule_survives_restart_and_rechecks_disk`.
This state is recovery material only: it grants no graph authority, and no
byte is written into the user's graph.

### 1.2a App-private immutable plugin packages

Installed plugin packages live below the app-data `plugins/` root as
`<plugin-id>/<version>/{manifest.json,plugin.wasm}`. The package is immutable:
same-version/same-bytes installation is idempotent, while the same version with
different bytes is refused. Manifest and capability policy stay in the Tauri
plugin layer; physical publication and removal use the certified
`tine-storage v0.12.0` package protocol.

Publication stages a complete directory at the store root under
`.install-<id>-<version>-<pid>-<sequence>`. Each file and then the staging
directory is synchronized before a native no-replace move installs the version;
Unix synchronizes both changed parents and Windows uses its certified
write-through name operation. Retirement durably moves the active directory to
`.retired-<id>-<version>-<pid>-<sequence>` at the store root before recursive
reclaim. Both transient grammars are disjoint from plugin ids because valid ids
cannot begin with a dot.

Every plugin-store open reclaims `.install-*` and `.retired-*` entries and any
active package directory lacking the exact two-file regular-file shape. This is
recovery, not interpretation: fully shaped packages still undergo the ordinary
bounded manifest, identity, symlink, and WebAssembly validation. Uninstall
first clears selection/settings through the audited settings replacement, then
retires package bytes. A crash at that seam therefore leaves cleared settings
and either a complete unselected package that can be retired on retry, or an
already retired/absent package; no state requires manual filesystem surgery.

### 1.3 Direct Files disposable graph projection

Direct Files stores one app-private
`direct-files-projections/<canonical-graph-path-digest>.sqlite` database outside
the graph. It contains only parser-derived physical page, block, task,
property, tag, path-ref, property-atom, and search facts; it contains no
authority stamp. Markdown/Org remains the sole Direct Files authority.

The database is `tine-storage`'s `PhysicalGraphProjectionDatabase`. Its
writable WAL uses `synchronous=NORMAL` and fresh schema DDL is one atomic transaction;
transaction commits are not authority or individual durability barriers, because
the file is a disposable cache (§3 invariant 3).

**Path refs and property atoms are one producer, not two.** `block_path_refs`
holds each block's reference closure — its own normalized refs, every
ancestor's, and the page's own normalized name — and `property_atoms` holds each
property element already flattened, de-duplicated by `atom_key` (first spelling
wins) and renumbered `0..n`. The SQLite projection and the in-memory tree walk
obtain these from the SAME closure and the SAME atomizer over the SAME per-block
input (`refs_norm`, the parser's own normalized refs). Neither may grow its own
copy that merely agrees by inspection: a graph has one behaviour, whichever
reader answers.

**Planning is not a task.** `block_planning` carries a block's `[#A]`,
`SCHEDULED:` and `DEADLINE:` facets for every block that has any of them,
written from the projection's three fields alone and never conditioned on the
task marker. `tasks` cannot answer these questions: a row is written there only
when a marker exists, so a markerless `SCHEDULED:` block is absent from it while
the tree walk still evaluates its date. The `scheduled_day`/`deadline_day`
columns hold the `yyyymmdd` ordinal and are NULL when the timestamp text is not
a calendar day, so a malformed date keeps its presence and loses only its day.

**The query columns are the exact visible text.** `block_text.query_visible` is the
block's visible text byte for byte and `blocks.query_visible_folded` is that
text canonically folded. Content predicates read the folded query text. The
`searchable_text` and its FTS stay whitespace-collapsed for the existing search
consumers and are not a substitute: a phrase query has to be able to tell `a  b`
from `a b`. Both columns are populated at WRITE time by the producer, never by
parsing or hydrating rows during a query.

**Query rows stay narrow (schema 26).** `query_block_results`
stores public result identity, tree preorder, construction estimate and tag/
property counts without duplicating raw payload. `block_own_refs` stores own
normalized reference names; `query_page_order` stores Direct session positions.
All are rebuildable facts in the page transaction with explicit FK-off cleanup.
Direct full reconciliation reuses projected facts for unchanged source revisions
and reconciles only the inventory-order table. Identical order performs no order
writes. Live page deltas capture retained/append/remove positions before queue
coalescing. The shared runtime-ID helper reproduces fresh-session IDs from page
path and structural order; live identity mappings and metadata-backed result
consumption are subsequent packets. Removing
global parsing from warm startup, streaming cold initialization, and result
consumption remain subsequent work; DB reuse alone does not claim fast end-to-end
startup. Direct source-revision table shape includes the schema26 marker so
older projection readers reject the newer disposable cache and rebuild it.

`pages` holds identity and routing;
`page_text` owns preamble and search text. `blocks` holds structure, metadata
and folded query text; `block_text` owns source content, original query-visible
text and search text. Keyed payloads are written in the same transaction as
owners and removed on replacement, deletion and reset. Existing typed payload
reads retain their fields; missing payload is an error, not an omitted entity.
Projection-only filtering does not load these payload tables.

**The result read is a descriptor read plus payload batches, and a damaged one
fails.** One ready query's public result is constructed from ONE owned read
snapshot, in two stages. The DESCRIPTOR read wraps the compiler's selected-id
relation and adds only ordering and result metadata — `query_block_results`,
the page's display fields, `blocks.order_key`, and `query_page_order.position`
— with no raw text, tag or property payload; every join in it is
LEFT, so a missing `query_block_results`, `blocks` or `pages` row, a Direct
result page with no `query_page_order` position, a `query_block_results.page_id`
that does not own its block's page, or a `pages.text_kind` outside the two
written values FAILS the read rather than dropping a selected descriptor. Cross-
page order is `query_page_order.position`, then `query_block_results.preorder`
within a page. The PAYLOAD read then runs for ADMITTED ids only, in batches of
128 bound ids and exactly three statements per batch — block/text/task/planning
facets, then `tags` by owner and ordinal, then `properties` by owner and ordinal
— never one statement per block and never a tags×properties join. Each batch is
validated for exact id coverage, page ownership, the stored tag/property counts
and, finally, the stored construction estimate against the estimate of the DTO
actually built; any violation abandons the WHOLE result with no partially
substituted rows. A read that fails this way is a rebuild request, never a
shorter answer (D-3).

**A tag key is a page key.** `tags.tag_key` is `refs::page_key(tag)` — the same
key page identity uses — because `#x` is OG's `[[x]]`; `tags_lookup_idx` leads
with it. `pages.journal_day` is the journal page's `yyyymmdd`, derived from the
page's own file stem under the graph's `:file/name-format` and journal formats,
and NULL for every other page.

**The projection has a statement seam; nothing else does.**
`PhysicalProjectionQueryReader` runs caller-supplied SQL against the graph
projection with bound parameters, plus `EXPLAIN QUERY PLAN` for the same
statement. Raw SQL crosses that boundary; **authority does not.** It is allowed
here, and only here, because the projection is a disposable cache derived from
the Markdown/Org tree: a malformed statement fails a read and can never corrupt
truth. The Markdown/Org tree keeps its curated typed boundaries and must never
gain such a seam.

The restriction is the **engine's**, not a validator's. The reader owns a
connection opened `SQLITE_OPEN_READ_ONLY` and no constructor accepts an existing
writable handle, so it cannot be reached from one; SQLite itself refuses every
write through it, which
`the_query_seam_can_read_the_projection_and_cannot_write_it` proves by
attempting `DELETE`, `INSERT`, `UPDATE`, `DROP` and `CREATE` rather than
assuming. There is deliberately **no** SQL-text parser or "single SELECT only"
check: it would be a runtime refusal with no in-scope failure to name — the
shape this contract's refusal table exists to keep out — and it could reject a
legitimate statement. Values travel as bound parameters in the signature, so an
interpolated statement is not expressible, and `explain_query_plan` binds the
same parameters as the query it explains, because with `sqlite_stat4` present an
unbound explain can report a plan for a statement the caller never runs.

**This seam adds no SQL-text refusal.** A missing or stale projection is caught
by lifecycle admission before a statement; a corrupt or otherwise failed read
is reported to the public dispatcher, which owns the one-repair rule below.
SQLite lock contention uses `busy_timeout`, while exhausted query-job capacity
is the separate typed `NotReady(Busy)` outcome.

**A query job owns its snapshot; the projection owns the jobs (R3).** A
database-answered query runs on `PhysicalProjectionQuerySnapshot::open_direct`,
one read transaction pinned for the job's whole descriptor-and-payload read so
every row it returns describes one projection state. The projection admits at
most `DEFAULT_QUERY_JOB_CAPACITY` jobs at once and **capacity is acquired before
the snapshot**, so a waiting job pins no WAL pages; the snapshot is validated
against the exact graph cache generation before and after SQLite establishes
the transaction, and a job whose generation moved is `NotReady`, never a stale
answer. Every admitted job registers its interrupt handle with the owner, and
**the worker drains every job before a rebuild touches the file**: it cancels
each registered statement, cancels waiters and late registrations, and blocks
until no slot is held, so no owned snapshot can retain a handle to a file about
to be reset or replaced (in-scope: a torn projection rebuilt under a live
reader). Closing the projection refuses every later admission. Cancellation is
a typed dispatch answer, not a failed read: it schedules no recovery or retry.

**Result identity follows who lowered the row.** `query_block_results.result_id`
is the runtime id the lowering process assigned. The projection tracks
`session_pages` — exactly the pages whose stored ids are LIVE in this process:
a full snapshot's replacements and each live save's page add to the set; a
structural relowering (a warm-stream replacement parsed in isolation) and a
deletion remove the page; dropping the parsed cache clears the set, because
the ids it held are no longer reachable. The set is captured with each
snapshot. A row on a session page answers with its stored id; every other row
(a warm reopen lowers none of them) answers with the structural runtime id the
shared helper reproduces from page path and structural order, which is the id
a fresh parse assigns it.

**Warm validation from bytes, never from a parsed graph.** Opening a Direct
Files graph validates the projection against the walk inventory and each
page's exact content revision computed from file bytes, parsing nothing. An
unchanged graph is READY with no parsed cache and nothing retained. A changed,
missing, damaged or config-mismatched projection names its replacement pages
and the caller streams them through a bounded high-water queue, parsing each
page in isolation and retaining none. Live saves and deletions enqueue their
delta whether or not a parsed cache exists, but readiness is published only
after this session has validated the complete inventory once (a full
snapshot, a clean warm, or a closed stream) — a delta alone never publishes an
inventory this process has not compared to disk. The `query_page_order` table
is reconciled by the worker from the queue's own page order whenever a stream
closes or a delta arrives without a position. In-scope scenario: an external
edit between two sessions, followed by a save of a different page before the
warm completes.

**One parse config, or a re-lowering.** Six graph-config facts decide those
derived rows — `:property/separated-by-commas`, `:ignored-page-references-keywords`,
`:block-hidden-properties`, `:journal/page-title-format`,
`:journal/file-name-format` and `:file/name-format`. Their digest is folded into
each page's `projection_source_revision`, and reconciliation compares only source
revisions, so a config edit re-lowers every page even though no file byte
changed. A mismatch is a benign, expected outcome of the user editing
`config.edn`, not corruption. That config travels **inside** each
queued Direct Files work item rather than beside the queue, so a page can only
ever be lowered and stamped under the config it was queued with: there is no
state in which queued work exists and the config describing it does not, and
therefore no way to stamp default-lowered rows as current.

Direct editor replacement briefly retains the old live inode as
`.<target>.<pid>.<sequence>.editor-recovery` and the proposed bytes as the
matching `editor-staged-recovery` name. Checked Direct Files open reconciles
only that complete producer shape through retained no-follow capabilities. If
the live target is absent and exactly one artifact claims it, that exact inode
is restored with no-replace. Multiple claims for an absent target remain in
place for explicit recovery; when a live target exists, every artifact is moved
unchanged to typed conflict trash. Every move rechecks the artifact's physical
identity and single-link status immediately before publication. A suffix
lookalike, symlink or reparse point, multiply linked file, ambiguous claimant,
or failed identity recheck is never deleted or selected as authority.

Every Direct Files create, live-name retirement, staged publication, recovery
restore, and recovery set-aside is one exact-byte name transition
(`move_graph_text_exact_no_replace`): the source must still hold the expected
bytes, the graph tree's own no-clobber rename publishes it
(`renameat2(RENAME_NOREPLACE)` through the raw syscall on Linux and Android,
`renameatx_np(RENAME_EXCL)` on Apple platforms, `FileRenameInformation` with
`ReplaceIfExists=false` on Windows), the parent directory barrier is required,
and the published name is re-read to prove what became visible. The staged
inode is flushed before it can become live. The typed
`DurableDirectoryPublication` boundary of `tine-storage` is NOT used on this
path: its Android arm is hard-link-then-unlink, which the FUSE-backed shared
storage a Direct Files graph lives in refuses (GH #466, v0.6.981: every
Android save failed with `Permission denied (os error 13)`);
`direct_files_graph_text_publication_uses_the_graph_tree_noreplace_rename` pins
that. Tine never acknowledges the save merely because an ordinary rename became
visible.
Successful replacement retires the displaced recovery name through a typed
`.editor-retired` name before deletion, so a crash cannot turn an unflushed
name transition into a reported durable save. That exact producer-shaped name
is cleanup-only: it never restores a document or becomes a conflict claimant.
The same bounded no-follow checked-open walk removes any copy left by a crash
or failed foreground unlink; a cleanup error fails that open without changing
the artifact and the next checked open retries it.

For an existing Direct editor save, the initial exact-file read supplies the
serialization baseline. The late external-writer proof is the atomic
retirement itself: after the expected physical owner is detached from the live
name, Tine reads that retained inode and compares it byte-for-byte with the
baseline before publishing. A mismatch restores the same inode when possible
and mints a conflict from the retained snapshot. There is no separate
pre-retirement full-file reread; creates and unpinned auxiliary writes keep
their independent recheck rules.

One background SQLite owner accepts either an already-resident
`PageEntry + Arc<Document>` snapshot or the bounded warm stream. The database
retains each page's exact caller-owned content revision together with the Direct
fact-extractor version as disposable adapter metadata. Bumping that extractor
version forces one background re-lowering when unchanged source bytes acquire
new physical facts. Warm validation compares the complete byte-derived source
inventory, parses only changed or missing pages in bounded batches and retains
no parsed graph; a clean reopen lowers none. One-page cache upserts and deletes
enqueue coalesced page deltas. The editor, watcher, and save paths never wait
for SQL. Indexed reads are admitted only when the worker has published the
exact current graph cache generation. One app-private sidecar lease permits
only one graph instance to publish into a projection database at a time, which
prevents an older instance from replacing facts behind another instance's
locally-ready generation watermark. A public query against a missing, stale,
corrupt, incompatible, leased or unwritable projection follows the typed
dispatch and bounded repair rules below; the remaining parser-owned navigation
and search consumers may use their existing fallback. Neither case blocks
graph open, save or external file observation.

The switched read families are literal fuzzy-search candidate
selection (including the `((` picker), and the original-case referenced-page
inventory used by autocomplete and navigation. They also include the shared
property-facet rows used by the query builder and editor autocomplete. The production
query route selects results through SQL. The switched families further include page aliases and
real-page ownership, explicit backlink and safely tokenizable unlinked-reference
candidate selection, persisted/runtime block-identity lookup, block-referrer
candidates, and distinct-referrer counts. Once current, these families
obtain a generation-bound
candidate/name set before applying the existing parser-owned matching and
presentation semantics. They no longer use manual whole-graph candidate scans
or second in-memory alias, reference-candidate, block-identity, referenced-name,
or block-ref-count semantic caches as their ordinary route.

TQL publication indexes its immutable captured documents with the existing
Direct projection producer only when an authorized page contains a TQL macro.
Setup failure aborts before publication replacement. The snapshot owns its
temporary root, and the writer retains that owner until its connection and
exclusive lease have closed. Expiration of the bounded close wait cannot remove
the root beneath an active writer. A real paused-writer test checks retention
through timeout, release at exit, and late registration after exit.
The root is created with mode0700 on Unix. Windows supplies a protected
inheritable owner-rights DACL at creation and reads it back before indexing;
unsupported or unexpected ACLs fail setup while the directory is still empty.
Existing directories are never reused or removed after a create collision.
The projection writer reports stale indexed reads without claiming that query
execution will fall back to traversal; public query adapters own typed recovery.
Bounded block-referrer results are semantically bounded, not merely count
bounded: candidate discovery covers the complete generation-bound candidate
set, groups it in relative-path/document order, and only then applies the row
and byte construction budget. `groups`, `total`, and `exceeded` therefore
describe one exact prefix; an internal-ID-ordered early subset is not an
allowed optimization.
The bounded generation-keyed memo of already-shaped frontend result DTOs
remains Tine-native for the reference-result families that still hydrate parser
DTOs; it is separate from the Direct public-query route below. For those
not-yet-migrated navigation and search families, an unavailable candidate or
name read may still use the established parser-owned fallback. Referenced-name
fallback walks only an already-parsed page cache and deliberately retains no
separate semantic memo. Non-UUID `id::` values and names that cannot be safely
narrowed by SQLite tokenization also use that parser fallback. Friendly graph
search likewise still ranks and produces evidence from parser-projected blocks:
the Direct projection narrows the single literal-fuzzy block shape when ready,
while the other Friendly shapes still use the parsed page source. These routes
remain explicit migration work; they do not authorize a fallback from the
simple, advanced, page, registry, or Explain public-query dispatch below.

**The Direct public-query route has no production tree-walk fallback.** A
semantically refused source returns its existing empty or unsupported-report
answer before any job or snapshot. Otherwise simple `{{query ...}}`, advanced datalog,
`@block`, `@page` and Explain reads enter `dispatch_direct_query`. The public
registry returns its published snapshot only when the graph generation, parse
config and declaration state still match; a refresh enters the same dispatcher
and query-job owner. A ready block query lowers once, wraps the selected ids in
the descriptor read and loads only admitted payload; a ready page query uses
the corresponding ordered page statement. Explain runs all of its probes
through one owned query job. There is no cost test and no selectivity hatch in
front of this route. The lowered selection chooses the ANSWER rather than a
candidate superset. Selection may inspect substantial predicate data; output
payload construction is restricted to admitted results.

Every other dispatch outcome is typed. Capacity pressure is
`NotReady(Busy)`. A worker that is already reconciling, rebuilding or applying
a delta reports its current `NotReady` reason. A stopped or unattached
projection is `Unavailable`; cancellation is `Cancelled` and schedules no
repair. An idle stale projection or an attempted failed read gets at most one
bounded repair and one SQL retry. If that retry still cannot answer, the caller
receives the resulting typed readiness or unavailability error. None of these
branches evaluates the parsed graph or fabricates an empty success.

**There is no fourth shape for "the compiler would not lower this."** The
lowering is TOTAL by type: it returns a statement for every query the IR can
represent, so no evaluable shape is routed to the walk by the compiler's own
choice. The two families that used to be excepted here — a valid `content
regexp` pattern (both the `content regexp <p>` spelling and a whole-query
`/pattern/`), and a `refs` nested inside a `children` quantifier — now lower like
any other predicate. Regex matches through a bound compiled-pattern ID that the
statement installs on the read-only seam and the seam evaluates against the
block's exact visible text; the pattern itself is never part of the SQL and never
logged. A pattern that does not compile stays a FALSE leaf, negation included,
exactly as the walk answers it — that is an answer, not a decline. Regex remains
deliberately UNINDEXED (there is no content index to bind it to), so it never
bounds an anchor by itself; a nested `refs` reads the ANCHOR's ancestor context
and so cannot bound its anchor either. Both facts are recorded as plan classes
rather than as exemptions, because an unbounded plan that is measured is
information and an unbounded plan that is excused is not.

Context-dependent child relations use a statement-local materialized map of
child and parent IDs. SQLite builds a temporary parent lookup rather than
scanning every child for every anchor. This setup can read all structural IDs;
it does not duplicate text or construct output payload. The durable schema is
unchanged. Regex program IDs distinguish the effective patterns of both
syntaxes, and missing required visible text fails the read.

**Direct current-snapshot reads.** Live queries do not capture saved-edit
targets, wait for coverage or require the projection to match the latest source
generation. Query execution reads the current complete SQLite image.
It uses the existing bounded producer capture queue to open the current complete
image, with actual SQL revision, session identity, parse configuration and
registry inputs captured together. Incomplete initial/rebuild images are refused;
ordinary queued edits do not make the committed image unusable. Full-text
readiness is read from the same transaction. Closure and replacement continue
to use the existing query-job lifecycle and cancellation owner.

**Committed-image refresh.** A Direct serving-image publication increments an
opaque notification counter after SQL, session identity, registry and readiness
publication, then wakes the existing application watcher control channel. The
watcher observes it per window binding and emits `query-projection-changed`.
The frontend rejects retired bindings and bumps ordinary dataRev, without
reloading live editors or changing page inventory. No query waits on or consumes
this counter; it is not a saved-edit target or answer-cache key. This covers
an early coherent query followed by later queued writes and rebuilds whose
physical SQLite revisions repeat. Inotify mode needs no periodic poll.

**Query answer ownership.** Direct simple and advanced queries
construct an operation-scoped answer from their acquired SQLite snapshots. The producer retains
no query answers and manages no answer-cache invalidation. SQLite owns database
page caching. Sharing result groups within an operation does not retain answers
across requests. UI retention of displayed results during refresh is independent
of this backend read path. Non-query reference caches keep their existing policy.
Repeated valid requests execute SQL again; only the displayed UI result may
remain mounted while its replacement is loading.

**What one dispatched query reads.** Capacity is acquired before SQLite opens
the owned snapshot. The snapshot is validated against the complete producer
image before and after its read transaction starts, then its
interrupt handle and the snapshot-scoped `session_pages` identity set are
registered with the job owner. A property-bearing Direct query captures its
committed registry cache alongside the SQL snapshot, using the actual storage
query revision and parse config. A query without a property leaf carries no
registry capture and does not clone dirty keys or scan the published registry
base; asking that registry-free job for registry state is an invalid-snapshot
error. This owner is separate from the editor registry.
The first read builds the registry from SQL; successful page deltas invalidate
only changed property keys and declaration-page dependencies. The producer
compares touched-page metadata before writing with its physical materialization
inputs, releasing the maintenance snapshot before the write. Text-only edits
with unchanged metadata reuse the registry without a full or per-key scan.
Full initialization, recovery, and config replacement invalidate the cache.
All turn writes and identity publication precede cache revision publication;
failed turns discard the owner. Query workers build or patch on their owned
snapshot, validating its actual revision even on cache hits. Older coherent
reads cannot overwrite newer publications or clear newer dirty keys. A
query without a property leaf uses the empty registry because no predicate can
observe its types. The answer then uses one descriptor statement plus payload
batches for the ids the budget ADMITS — never a candidate superset, never a
page the answer does not contain, and never a page document. Ready query selection
and result construction load NO `Document`, read NO source text and consult no
parsed graph. Recovery source-inventory work is counted separately. The page's name, kind and journal day
come from the projection's own page row, and a projection that cannot account
for a selected descriptor fails the whole read rather than serving a shorter
answer.

**A block statement answers `(block_id, page_id, path)` and nothing else, and
its match set reads `blocks` alone.** The block identity is the answer, the page
id is the stable routing identity, and the
path is the Direct Files order key the descriptor read joins on; the page's
name and kind come from the descriptor read's page row for the ANSWER, never
from a column carried alongside every candidate row. The materialized production spelling joins `pages` for result routing
only on the ANSWER — after matching and after the
result-set rule has dropped a match whose immediate parent also matched — because
a page predicate carries its own `pages` subquery keyed by `page_id` and
`blocks.page_id` is a NOT NULL foreign key, so joining during candidate
selection could neither add nor drop a match. Both halves are load-bearing:
carrying two unread columns and probing the pages primary key once per candidate
is the unrequested work the projection exists to remove, and it measured 0.81 –
0.98× of the previous statement across broad and selective nonempty shapes on
the anonymized corpus, with controls recorded in `RECEIPT-db1.md` (under 1% for shapes above
25 microseconds). Page-anchored statements are unaffected and still answer
`(page_id, name, text_kind, journal_day, path)`. A row that does not have its
required shape is a failed read, never an empty answer. The lowered selection
statement itself has no ordering metadata. Its descriptor wrapper does: Direct
block answers carry `query_page_order.position` and
`query_block_results.preorder` and end with `ORDER BY` on those columns; the
page wrapper carries the same page position. Missing Direct order metadata
fails the read, because silently moving a page would change which rows survive
a bounded budget.

Page results carry physical graph-relative `path`, name, kind, optional journal
day and authored ordered properties. Their shared descriptor wrapper applies
the complete saved sort and `COUNT(*) OVER()` before its row limit. Unicode
text sorting reuses Rust lowercase through operation-owned rank callbacks;
numeric-looking property values remain lexical. Explicit sort ties use physical
path, while unsorted reads retain Direct inventory order.
`query_page_results` supplies raw construction estimates and property counts;
only admitted owners receive property payload reads, in batches of 128, with
ownership, ordinal, count and estimate validation. Both row and byte limits
apply. `total` counts admitted pages before sampling; optional `matched_total`
reports the complete SQL match count, independent of limits and sampling.
Page navigation uses the physical path even when display names coincide.
External watcher reconciliation uses the ordinary page-delta producer even
when the full parsed cache is absent. Repeated delivery of an already admitted
revision/config uses the existing session identity record to avoid another
delta; no query answer or saved-edit target is retained for this purpose.
Recency selection may read file metadata and is measured separately from
output payload. Cancellation during selection or hydration returns no partial
page answer. This does not yet supply complete SQL grouped statistics.

The FTS-readiness signal the content predicates' bounds
depend on is probed through the same seam and remembered once per generation,
never once per query — and only a READY observation is remembered, because
readiness is monotonic within one projection file while a rebuild publishes a
new generation.

**A failed read repairs once, then reports the SQL outcome.** The in-scope
scenarios are §3.1's: a torn or truncated projection file after a crash or power
loss, a disk error, a resource limit, or a projection whose page set has drifted
from the current graph generation. Because the projection is disposable,
`dispatch_direct_query` requests a rebuild only after the failed job and its
owned snapshot have dropped, then retries the SQL route once. The worker drains
all old query jobs before it resets the file. If a complete parsed snapshot is
already resident, recovery may enqueue it; otherwise recovery validates a
complete source inventory from bytes and streams bounded per-page replacements,
without constructing or retaining a whole parsed graph. Clearing readiness
alone would strand the projection until another edit. Cancellation is excluded
from repair because the drain or close deliberately removed the snapshot's
subject.

Production queries construct no candidate-page plan and apply no
selectivity cutoff. A simple query is parsed once; invalid input returns its
semantic empty refusal before any snapshot or registry acquisition, while valid
IR enters the captured SQL route. Candidate types and lowering remain test-only
for the independent oracle. Production metadata cursor reads and oracle lowering
share the unchanged `query_cursor::drain_after` advancement and batch-retry owner.

Copy/export query subtrees use the current-main snapshot boundary and one SQL export executor. Root/node/byte limits remain cumulative over the command. The previous export walkers exist only as test oracles. Friendly public search and print-query rendering remain separate campaign consumers until their SQL migration is accepted.

Static whole-site publication uses one operation-owned main snapshot and the shared query resolver/compiler/readers for every authored query occurrence; it has no answer memo or captured-document query database. Direct compares the complete captured path/content-revision/config fingerprints to `direct_source_revisions` inside that read transaction, refusing a mismatch before staging output. These fingerprints are disposable derived data, not source authority. Its fresh capture uses structural result IDs without changing the live editor identity owner.

The whole-site renderer already owns captured documents for page output, embeds and namespaces; admitted query roots hydrate from that same capture. Visibility filtering happens after complete matching, and ambiguous canonical page identities remain refused. Page-result links require matching physical path, title and kind in the captured public capability. Cancellation is checked before the atomic output-stage commit. Publication tests exercise real main SQL, private omission counts, advanced/TQL/BEGIN_QUERY/query sheets, ordinary external source progression, capture identity isolation and output-stage recovery.

## 2. Graph-tree publication and durability

This section states how Direct Files makes graph-tree writes durable, and what
it does when a filesystem cannot provide a primitive. The ordinary page save
keeps its audited shape: temp + fsync + exact durable name publication +
base-revision guard + lock (§1.3 describes the name transition).

### 2.10a Durability barriers are strict on every platform

Every durability barrier on the graph tree is strict on **every** platform,
Android included. Page text, conflict copies, trash, withdrawn bytes and assets
have no second copy to rebuild them from, so a barrier the filesystem refuses
is a real durability failure and fails the write
(`model::sync_projection_directory`).

The one tolerance left is the Direct move-recovery journal's directory barrier,
`filesystem_durability::sync_move_recovery_directory`. On Android only, and
only for `PermissionDenied`/`Unsupported`/`InvalidInput`
(`EPERM`/`ENOTSUP`/`EINVAL`), that barrier accepts the refusal; every other
errno stays fatal, and every other platform stays strict. Android CI run
32088229039 recorded shared storage refusing the directory flush with
`detail:Invalid argument (os error 22)`. Managed Storage additionally had a
degradable artifact class for its reconstructible Markdown projection; it was
removed with that mode (ADR 0066).

Because the device is the only oracle for these semantics, every platform
primitive on the graph-tree publication leg — the directory flush, `renameat2`
with `RENAME_NOREPLACE`, file `fsync`, and the no-follow `openat` of a parent or
file — names its operation and its location in the error it returns.
`ErrorKind` is preserved, because guarded-conflict classification and the
journal's Android tolerance both match on it.

The journal tolerance is enforced by
`filesystem_durability::tests::android_tolerates_only_the_three_capability_refusals`.

### 2.10a-i Durability barriers and the batch commit point

A **durability barrier** is a syscall that forces bytes to stable storage:
`fsync` of a file, `fsync` of a directory, or `syncfs` of a filesystem. Each one
is a device round trip. Their *count per operation* — not the time any single
phase reports — is what turns an ordinary edit from milliseconds on a local SSD
into hundreds of milliseconds on a slow or network filesystem, and it is
invisible to phase timers because the multiplicity is spread across modules.
Direct Files has no multi-artifact batch: its commit point is the exact durable
name publication of one page (§1.3).

`crate::durability_counters` counts every barrier `tine-core` initiates, and
`durability_counters::tests::production_barrier_primitives_stay_inside_counted_wrappers`
rejects a raw barrier outside the counted wrappers.

**Read paths take no barriers.** `fsync` before reading a file defends nothing:
a read is served from the same page cache the writer wrote into, so forcing
those bytes to the platter cannot change the returned bytes, and it cannot
detect corruption either. The three such helpers are deleted (§3.1's
removed-checks table), and
`durability_counters::tests::no_read_path_reintroduces_a_durability_barrier`
fails if one returns.

**A publication flushes the leaf directory of its parent chain, and nothing
above it.** `sync_projection_chain` flushes `chain.last()` only. The
leaf-only argument has two halves and they are exhaustive:

* An ancestor **Tine created during this operation** is made durable when it is
  created, not afterwards: `model::create_projection_chain_component` is the
  only place a chain component is created, and it flushes the parent that now
  holds the new name before the chain builder descends into it. A crash between
  the `mkdir` and the operation's own barrier therefore cannot lose the path the
  operation is about to publish into
  (`model::tests::projection_retry_resumes_after_synced_partial_parent_chain`
  drives exactly that crash point and proves the retry converges).
* An ancestor **Tine did not create in this operation** already had a durable
  entry in *its* parent before the operation began. No in-scope scenario
  un-durables an entry that is already on stable storage: crash/power loss,
  torn write, disk error, sync-service delivery, external-editor race, honest
  concurrent instance, honest multi-device divergence and malformed imported
  content can all destroy or replace such a directory, but none of them can be
  repaired by this process re-issuing `fsync` on it, and every one of them is
  already handled by the guarded-conflict, no-follow and recovery machinery
  above. The removed flushes are removed because **no in-scope scenario needs
  them**, not because they were expensive — the refusal-scenario rule in
  `AGENTS.md` §5 cuts both ways, and a barrier with no scenario is latency the
  user pays for nothing.

The one failure the removed flushes did cover is **another** process creating an
ancestor directory and not flushing it itself — a durability obligation that
belongs to that writer, that Tine cannot discharge on every subsequent write
without paying the barrier forever, and that Linux's ordered metadata journals
largely subsume anyway (a directory `fsync` commits the transaction that created
its parent). It is recorded here rather than defended.

Direct Files' user-visible Markdown publication keeps its
temp + fsync + exact durable name publication + base-revision guard + lock.
The platform-specific typed publication boundary supplies the name-durability
guarantee, including write-through publication on Windows.

### 2.10b No-clobber publication when the filesystem has no rename flags

The directory barrier is not the only primitive Android shared storage refuses.
Android CI run 32091898520 recorded the **flagged rename itself** failing with
`Invalid argument (os error 22)`. Two earlier lanes eliminated this call by
reading AOSP `FuseDaemon.cpp`, whose `do_rename` accepts exactly that flag. The
device disagreed. `RENAME_NOREPLACE` has to be provided by every layer — the
kernel FUSE client, the daemon, and the filesystem underneath it — and on this
path it is not. **Upstream source is evidence about upstream intent, not proof
about the running device; the receipt wins.** The same `EINVAL` is reachable off
Android on any filesystem without `rename2` flags (FAT/exFAT removable media,
some FUSE and network mounts).

Every graph-tree name transition therefore takes the platform primitive and
nothing else, on every platform, through `model::rename_projection_noreplace`.
Graph text is sole-authority data: a two-step publication (reserve the
destination with an exclusive create, then rename onto the reservation) could
leave a reserved-but-empty file at a live graph name after a crash, with no
second copy to rebuild it from. On a filesystem without the flag, a Direct
Files create or save fails rather than publishing non-atomically, and the error
names the refused call. Managed Storage's reconstructible projection used that
reservation fallback, with a per-device memo of the answer; both were removed
with it (ADR 0066).

### 2.10d When the graph filesystem folds two page names into one file

Android CI run 32123012366 recorded a graph fixture refusing to write itself
on real shared storage
(`/storage/emulated/0/Download/…`):

```
journey graph fixture could not be written: graph filesystem folds two journey
page names into one file: pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md reads
back the bytes written for pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md
(18 bytes, not 8)
```

Two files whose names differ only by case cannot both exist there. This is not
confined to Android: FAT/exFAT removable media, NTFS, APFS in its default
configuration and any `ext4` directory carrying the casefold attribute fold
case, and HFS+ additionally folds Unicode normalization.

**Which folding.** Three axes are separable platform facts — ASCII case,
non-ASCII (Unicode) case, and NFC against NFD — and a graph that is legal under
one is illegal under another. On the API-35 emulator the answer was **case folds,
normalization does not**: the fixture verifies its shapes in list order, and the
run above reported the case pair while the normalization pair
(`pages/\u{17d} pilot notes #pilot.md` against
`pages/Z\u{30c} pilot notes #pilot.md`) had already read back byte-exact.

AOSP disagrees with that. Android shared storage folds case through
`ext4`'s casefold attribute, whose comparison (`fs/unicode`, `utf8_strncasecmp`)
is defined over the NFDICF form, and NFC and NFD share that form — so on the
source, normalization should fold too. §2.10b already settled how that
disagreement is resolved: **upstream source is evidence about upstream intent,
not proof about the running device; the receipt wins.** The contract below
therefore holds whichever of the three axes a device folds.

**Why this is not, by itself, a merge of two pages.** Tine's logical page name
is already case- and normalization-insensitive: `LogicalPageName::key_digest`
hashes `canonical_page_name_key`, which lowercases and then applies NFC,
matching Logseq. Every pair of file names a case-folding or normalization-folding
filesystem cannot tell apart is therefore a pair Tine **already treats as one
page**. Such a filesystem cannot merge two distinct Tine pages, because two
names it folds were never two pages here. This is the load-bearing fact behind
everything below, and it is bound to the code by
`refs::tests::filesystem_folding_never_separates_names_tine_already_treats_as_one`.

What folding does change is that a duplicate file — a second spelling of a name
the graph already holds — cannot exist there at all. Whoever wrote the second
spelling (a sync client, a file manager, the user) overwrote the existing file
instead of landing beside it.

**The contract.**

| | On a folding graph filesystem |
| --- | --- |
| Pages | Exactly ONE page per folded name — never two, never none. The twin spelling never becomes a second page, and never displaces the first. |
| Bytes | The page carries whatever the storage actually holds. An outside write to the twin spelling IS a write to that one file, so it reconciles as an ordinary external edit. |
| Writes | Tine writes a graph path only when it either learned that exact path from the filesystem's own directory entry or creates it through the no-clobber publication (§2.10b), so Tine can never be the writer that destroys a folded twin: an occupied fold resolves to `AlreadyExists` before anything has moved. |
| Reporting | A fold performed by ANOTHER writer before Tine ever saw the graph is not detectable and is not reported: Tine has no evidence two files ever existed. |

**No probe.** Tine does not detect which axes a graph filesystem folds, because
no behavior depends on the answer: the contract above holds on every
filesystem. (A write/read-back probe existed for the Android shared-storage
journey and was removed with Managed Storage, ADR 0066, having no production
caller.)

**What is deliberately NOT promised.** Tine does not reconstruct a side of a
folded pair that another writer already destroyed, and does not claim a merge it
has no evidence of. On such a device the user's graph can hold only one of the
two spellings; keeping both requires a name that differs by more than
capitalisation or accent spelling.

Enforced by the equivalence-class test above and by the no-clobber publication
tests of §2.10b.

### 2.10f The interrupted-publication recovery walk

Editor publication uses process-scoped recovery names rather than names
derivable from durable state (I2b). The parser accepts
`.{target}.{pid}.{seq}.editor-recovery` and the four-field
`.{target}.{pid}.{seq}.{turn8}.editor-recovery` shape. It is a parser, not a
suffix glob: the leading dot, numeric process fields, hexadecimal turn field
when present, known suffix, and text-extension target must all validate.

Every checked graph open performs one bounded, no-follow interrupted-publication
walk before the graph is served. The walk uses the graph-text inventory limits,
reads no document contents, restores a sole claimant over a missing live name
with no-replace, and moves every competing claimant to conflict trash.
Traversal, bounds, permission, and unsafe-entry errors propagate through
`open_checked`; they may not be converted into an empty result. This is I2c: a
failed walk refuses the open.

`model::tests::checked_open_fails_closed_when_the_recovery_name_walk_exceeds_its_bound`
and `model::tests::editor_recovery_names_accept_legacy_and_turn_derived_shapes`
enforce the bounds, grammar, and API propagation.

### 2.10g Retain-never-delete recovery

Recovery never treats a graph-tree object as scratch. Anything unbound, changed,
occupied, or merely byte-identical under another identity is moved intact to
the graph-local conflict trash (`logseq/.tine-trash/conflicts/`); it is never
unlinked. The one cleanup-only name is the typed `.editor-retired` name (§1.3).

On Windows, backup restore's capability-bound move to graph-local recovery uses
hard-link-create followed by source removal, never check-then-rename. If a sync
service such as Syncthing or Dropbox delivers the same recovery name between
observation and publication, hard-link creation returns `AlreadyExists`; the
delivered entry is not replaced and the original live source remains. Linux,
Android, macOS, iOS, and Windows are explicit compile-time arms; an unknown
target cannot inherit a Tine platform's publication policy by negated fallback.

## 3. Invariants and versioning

Invariant numbers are stable because code cites them by number; a retired number
keeps its place.

1. The threat is crash, power loss, torn write, and interrupted/reordered file
   sync—not a malicious byte-forging actor. Content digests detect accidental
   damage and name content; they are not a security authenticator.
2. The Markdown/Org tree is the sole authority for page and journal content,
   IDs, names/paths, references, and properties. Assets, PDF sidecars,
   `config.edn`, and app settings retain their separate authorities. Merely
   opening a PDF reads its asset-side state and does not create an empty
   semantic `hls__` page; the first annotation write creates or updates that
   page through the paired sidecar and page-save path.
3. SQLite projections, the Concord base ledger, and other app-private derived
   state are disposable. Deleting or version-mismatching one may cause exactly
   one bounded rebuild, never a second rebuild on the following open.
4. Graph bytes are replaced only through the audited publication path, under
   the base-revision guard and page lock (§1.3, §2). A cache cannot authorize a
   Markdown overwrite.
5. *(Retired 2026-09-15.)*
6. *(Retired 2026-09-15.)*
7. *(Retired 2026-09-15.)*
8. The app-private graph-fact projection contains no authority state and grants no
   authority.
9. Graph-text writes take two locks, and always in this order: the
   **graph-text identity-mutation gate** (`GraphTextWriteGate::lock_identity_mutation`,
   graph-global, exclusive across threads, re-entrant per thread) first, then the
   **per-page lock** (`Graph::page_lock`, per path). A writer that holds a page
   lock and then reaches the gate deadlocks against every writer that takes them
   the other way round — and because the gate is graph-global and its holder is
   blocked, the whole process stops publishing graph text, not just that page.
   The order is a static property of the code and is proved statically, by
   `graph_text_writers_take_the_identity_gate_before_any_page_lock`, which walks
   `model.rs`'s call graph and fails on any function holding a page lock that can
   transitively acquire the gate. `debug_assert` cannot enforce it: the shipped
   release profile compiles those out, so before 2026-09-01 the release binary
   reached the deadlock where debug builds reached an assertion.
   Reading the resource epoch (`identity_mutation_epoch_under_authority`) requires
   the calling thread to hold the gate; it now refuses with
   `graph_text_admission_unavailable` rather than reading a value another thread
   is free to advance. That is an internal precondition, not a threat-model
   refusal, and no in-scope scenario reaches it once the static order holds.
10. *(Retired 2026-09-15.)*
11. *(Retired 2026-09-15.)*

### 3.1 Refusal scenarios

Every durable storage refusal names the in-scope failure it defends against. A
transient condition that is safe to retry is not a durable refusal; a disposable
cache failure must rebuild instead of appearing in this table. The `MS-REF-`
prefix is historical; scenario IDs stay stable.

| Scenario ID | In-scope failure | Required response |
| --- | --- | --- |
| `MS-REF-UNSAFE-FS-KIND` | Sync delivery, filesystem damage, or an external tool replaces an expected directory/regular file with a symlink, special file, reparse point, or unexpected hard-link alias | Refuse access through the substituted entry without following it |
| `MS-REF-BOUNDS` | Honest corruption or malformed imported input exceeds explicit memory, depth, count, or byte bounds | Reject before unbounded allocation or traversal and report the bounded class |
| `APP-REF-PLUGIN-IMMUTABLE-COLLISION` | Two honest concurrent installs, or a crash-recovered retry racing a completed install, present different bytes for the same plugin id and version | Keep the no-clobber winner byte-exact and refuse the other install as `immutable plugin version ... different bytes`; never overwrite or merge the package |

#### Checks with no in-scope scenario, and what happened to them

The rule that every refusal, fail-closed path, or re-verification of already
established state must name a concrete in-scope failure applies to *silent*
defensive work too. A barrier or re-proof that cannot name a scenario is not
hardening; it is unpaid latency, and later a source of availability bugs.

| Removed check | Where it was | Scenario it claimed | Why it has none | Replaced by |
| --- | --- | --- | --- | --- |
| `fsync` before reading a projection evidence file | `model::sync_and_read_projection_regular` | — | A read through the same process's page cache returns the bytes the writer wrote whether or not they are on the platter. Flushing cannot change the result and cannot detect corruption. | Plain bounded read (`read_projection_regular`) |
| `fsync` before opening-and-reading a projection file | `model::sync_open_and_read_projection_regular` | — | As above. On Windows it additionally forced a write-capable open for a read. | `open_and_read_projection_regular` |
| `fsync` before re-reading a retained quarantine handle | `model::sync_and_reread_retained_projection_file` | — | As above; the handle is the one this process just wrote through. | `reread_retained_projection_file` |
| `fsync` of every **ancestor** of a projection target's parent chain | `model::sync_projection_chain` (then a leaf-to-root loop), reached from ~30 write/rename/preflight call sites | — | The operation changes entry lists in the chain leaf only. An ancestor Tine created in this operation is already flushed by `create_projection_chain_component` at creation; an ancestor it did not create already has a durable entry in its own parent, and no in-scope scenario (crash/power loss, torn write, disk error, sync delivery, external-editor race, honest concurrent instance, honest multi-device divergence, malformed import) can un-durable an entry already on stable storage. See §2.10a-i for the one out-of-ownership case it did cover. | One barrier on the chain leaf, plus the existing per-creation barrier |

The removed barriers are replaced by nothing because nothing needed them;
integrity of graph bytes is still checked by the means that actually detect
corruption, the exact-byte rereads of §1.3.

## 4. Concord base ledger (Direct Files)

The Concord base ledger (ADR 0056) is **disposable state**, in the invariant-3
sense: app-private, derived, safe to delete wholesale at any time. It lives
outside every graph tree at `<app_data>/concord-ledger/<root-id>/` (the
backups' root-id convention) and stores, per graph-relative page path, the
last text Tine successfully read from or wrote to disk — sha256-addressed
blobs plus a path→hash index and conflict-copy pins (schema
`concord_ledger::LEDGER_SCHEMA`, currently 1).

It is never an authority: nothing validates against it, no refusal scenario
consults it (§3.1 is unchanged by its existence), and its loss or corruption
changes exactly one behavior — sync-conflict diffs degrade from 3-way with
pre-selected suggestions back to the plain 2-way diff until the ledger
repopulates from ordinary saves and admissions. Ledger updates are best-effort
background work off the save critical path; they may not block or fail an
open, save, or reload. Quitting Tine waits at most
`concord_ledger::EXIT_DRAIN_BUDGET` (200 ms) in total for every open graph's
queued updates, because quitting right after a save is the ordinary
multi-device case the ledger serves; an update still queued after that is
lost, and that page's next conflict merges against an older base.
Each graph's ledger is its own directory, keyed by the graph's root id.

A prune runs at graph open and reclaims everything that can no longer answer:
blobs referenced by no index entry and no pin, index and pin files that do not
parse, and index and pin entries naming a blob that is absent. `record` writes
a blob before the entry that names it, so an entry without its blob means the
blob was removed from outside the ledger — antivirus quarantine, a disk
cleaner, a partial restore. Such an entry is dead metadata whose lookups
already answer `None`; reclaiming it is hygiene, and the ledger never warns,
refuses, or reports a missing blob to the user.
