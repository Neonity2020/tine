# Managed storage and sync contract

This document is the implementation contract for Tine's opt-in managed-storage
runtime. Direct Files is the default product path and selects a mutually
exclusive `Legacy(Graph)` runtime before graph open. When Direct Files is
selected, no code below may inspect or modify `.tine-sync`, open an oplog,
create managed scratch state, or start managed recovery.

One native `StorageModeSupervisor` owns storage-transition identity, priority,
serialization, and terminal outcomes. A transition has a monotonically
increasing operation ID, exact window and canonical graph root, typed kind and
phase, and exactly one native terminal outcome. Late work may publish only while
its operation remains current. Long work is never serialized app-wide:
different canonical roots have independent lanes, and graph-slot publication is
a short current-operation compare-and-publish. A stuck graph cannot block an
unrelated graph or later overwrite the newer selection in the same window. The
frontend renders events for the current operation ID; phase-name prefixes,
frontend attempt tokens, and inactivity timers are not storage authority.
Supersession is cooperative abandonment, not forcible thread cancellation: a
blocking OS worker may finish disposable computation, but it cannot publish or
change the selected mode after its operation ID becomes stale. Operation start
and final graph-slot publication share one short linearization lane; neither a
root lane nor the supervisor model mutex is held for graph-sized work. Managed
activation and join use a move-only publication guard: preparation can produce
exactly one published successor, and the post-publication guard has no API for
publishing again. Graceful Direct Files recovery uses a separately typed
multi-step guard which managed activation and join cannot acquire.

Installing a native graph slot is not sufficient evidence of stable readiness.
A managed candidate is reported successful only after its actor-backed
generation opens the exact accepted-frontier-stamped SQLite materialization,
answers its complete paged inventory, and opens deterministic representative
pages including the largest captured source page. The largest-page path is
retained during the activation capture; readiness never walks the Markdown tree
a second time. The resulting structured native receipt names inventory, sample,
and total timings. Only then may one native publication replace the exact
predecessor generation and persist the Managed selector. The frontend reacts to
that terminal native receipt by retiring renderer state; it does not re-prove
native readiness.
The SQLite candidate receives that stamp only after its authenticated page
catalog was covered exactly once and every page was materialized.
Readiness never compares against a cached Direct Files inventory captured before
the transition or the actor's live current-path catalog: filesystem delivery may
legitimately change either while startup catch-up is settling. The accepted
frontier's raw document count is not a page count because it also includes
non-page managed documents. An empty graph
legitimately proves readiness with an empty inventory.

Explicit activation is never an unexplained spinner. Before native activation,
the frontend names pending-save flush, confirmation, and progress-listener
setup; during activation it renders the active native operation and detailed
native construction progress. Fresh bootstrap reports its construction phases.
Reactivation of retained clean state separately reports marker/baseline/index
open, committed-tail replay, projection repair, and actor open. After native
success the frontend rebinds its renderer to the generation named by the native
result. These progress values are observational and cannot authorize
publication.
After managed slot publication, the watcher still performs one full handoff-gap
reconciliation. It begins immediately, but the expensive path-comparison phase
is a retained cursor with both a path-count and wall-time budget per actor turn.
That comparison reads exact accepted projection bytes; it does not replay the
parser and semantic mutation planner for unchanged pages. A differing path is
only a candidate: the ordinary external-reconciliation transaction must still
reconstruct and validate its complete semantic predecessor before authoring.
The cursor remains visibly pending and prevents `Safe` until its exact epoch is
settled; application, enrollment, and status requests can run between turns.
There is no timer-based priority claim and no O(graph) comparison turn on the
shared actor lane.

Return to Direct Files has two meanings. A graceful return drains a healthy
managed actor and confirms its committed projection before selecting Direct
Files. An emergency return is always available from managed startup/refusal.
The explicit recovery button invokes it immediately without a native
confirmation dialog that could be delayed by the failing managed open. It
atomically retires the private managed selector and opens the current
Markdown/Org tree without first opening, repairing, draining, archiving, or
recovering managed state. Managed evidence remains quarantined for inspection,
and the UI warns that it may contain operations newer than Markdown. Re-enabling
managed storage after emergency return starts from the then-live Markdown tree;
quarantined authority is never silently resurrected. Emergency return
supersedes in-flight managed open/activation/join at native safe checkpoints,
and an older operation cannot publish a managed slot afterwards. The ordinary
Settings action is always graceful: if drain or projection confirmation fails,
it leaves managed evidence selected and offers the separately named emergency
return rather than force-stopping managed authority implicitly.

The authoritative layout names live in the pinned
`tine_storage::formats` manifest. Core code imports them through the
definition-free compatibility surface in
`crates/tine-core/src/oplog/sync_layout.rs`; it must not introduce another
literal. Format/schema constants remain beside their codecs and are likewise
certified through `tine_storage::formats`.

[ADR 0054](adr/0054-lazy-genesis-managed-activation.md) is the sole production
activation format. Existing pre-0.7 enrollment and multipart-bootstrap state
is refused as authority and the product offers Return to Direct Files before a
fresh clean activation. The old constructor and same-process handoff remain
callable only under `cfg(test)` as a bounded differential oracle while their
source modules are physically retired; no production open, activation, or
actor thread can enter them. A partially
implemented genesis artifact is never authoritative: only the final clean
activation marker selects the baseline-plus-manifest runtime. The exact
removal/replacement ledger is
[managed-activation-authority-census.md](managed-activation-authority-census.md).
Every constructed lazy-genesis candidate nevertheless carries the exact causal
`DocumentDependencies` for its catalog and page checkpoints. Installing that
candidate into the shadow engine constructs a sequence-zero accepted frontier
whose constant-size genesis binding commits the sealed manifest root. Its
accepted-document map is an initially empty overlay containing only causal rows
superseded by later operations; it does not copy the graph-sized genesis map.
Subsequent accepted operations must preserve the genesis binding. Test-only
legacy fixtures are not an alternate activation marker or permission to admit
a partially constructed candidate.

Production sharing likewise has one route: clean activation publishes a clean
baseline descriptor and clean joining installs that exact baseline plus its
manifest tail. The pre-0.7 share and join implementations compile only in
tests. A production decoder may still recognize an old descriptor so it can
give a bounded migration refusal, but it cannot use that descriptor to reopen
the retired runtime; the user must Return to Direct Files and share again.
Both successful enrollment cuts intentionally retire the actor that entered
them. Tauri must reopen the durable result, prove ordinary page inventory/load,
and atomically replace the exact predecessor graph slot before reporting share
or join success; querying or continuing to serve the retired actor is invalid.

That retirement is recorded on the handle by the cut itself, not left for the
caller to infer. `SyncRuntimeHandle::prepare_shared` on success, and
`join_shared` on completion, close the private sender and join the actor thread
before returning, so the handle's final published snapshot becomes its
authority. The two observational calls that survive a retired actor therefore
keep working: `status()` reports the actor's own last snapshot, and
`clean_shutdown()` reports `Safe` for a runtime that already reached
`StoppedSafe` — which the cut guarantees, because it commits the Safe
transaction before publishing anything. Every other request on a retired handle
is `ActorUnavailable`, which is the truth. A pre-Safe refusal from either cut
retires nothing: the actor stays reachable for an explicit retry or a
crash-style drop.

This is an availability rule, not a durability concession. `clean_shutdown`
still reports `Safe` only from a `StoppedSafe` lifecycle; it never converts an
unreachable actor, a terminal latch, or outstanding work into `Safe`. Reporting
a bare `ActorUnavailable` for a state the runtime durably reached serves no
in-scope threat, and off-host it costs a full CI round trip to localise
(Android CI run 32098261560: `clean shutdown failed: Err(ActorUnavailable)`
immediately after a successful share preparation). The Android instrumentation
receipt accordingly prints the runtime `status=` beside every save or shutdown
refusal, so an unreachable snapshot is distinguishable from a reported one.

## 1. On-disk layout

### 1.1 Shared graph-local provider

The complete graph-local managed namespace is `.tine-sync/v2/shared/`. It is
provider transport, not the application's local database. Each device writes
to `outbox`; a file-sync provider delivers those immutable files into another
device's `inbox`. Tine tolerates temporary and reordered delivery and does not
interpret the mere presence of the directory as an opt-in marker.

| Relative path under `shared/` | Writer | Reader | Format | Lifecycle |
| --- | --- | --- | --- | --- |
| `inbox/`, `outbox/` | transport scaffold | `SharedProviderTransport` | directories | created on explicit activation/join; retained |
| `{inbox,outbox}/enrollment/shared-enrollment-v1.json` | initiator | cold discovery and joiner | clean magic-prefixed descriptor v1; legacy JSON is recognized only to refuse pre-0.7 state | immutable identity for the shared graph |
| `{inbox,outbox}/clean-baselines-v1/<root>.index` | initiator | clean joiner | canonical lazy-genesis provider index v1 | immutable; descriptor-bound; published after every baseline chunk |
| `{inbox,outbox}/clean-baselines-v1/<root>.<file>.<chunk>.chunk` | initiator | clean joiner | fixed-size exact chunk of a sealed lazy-genesis file | immutable; reassembled only through the descriptor-bound index |
| `{inbox,outbox}/objects/<digest>.object` | publishing device | peer ingress/replay | immutable oplog object envelope | append-only; digest-addressed |
| `{inbox,outbox}/manifests/<batch>.manifest` | publishing device | peer ingress/replay | canonical batch manifest | append-only commit object |
| `{inbox,outbox}/frontier-heads-v1/<device>-<digest>.head` | each device | peer discovery | canonical JSON frontier head v1 | immutable heads; newer generations supersede discovery relevance |
| `{inbox,outbox}/publication-intents-v1/<digest>.intent` | publishing device | interrupted-publication recovery | canonical JSON intent v1 | immutable; retired only after covered publication is proven |
| `{inbox,outbox}/manifest-recovery-links-v1/<batch>.link` | publishing device | peer recovery | canonical JSON recovery link v1 | immutable |
| `{inbox,outbox}/manifest-recovery-blobs-v1/<digest>.manifest` | publishing device | peer recovery | exact manifest bytes | immutable; digest-addressed |
| `{inbox,outbox}/.part/` | provider transport | provider transport | temporary publication bytes | disposable after recovery |
| `{inbox,outbox}/removed/` | provider transport | provider cleanup/audit | retired provider items | bounded cleanup evidence |
| `{inbox,outbox}/rename-evidence/` | provider transport | provider recovery | interrupted-rename evidence | disposable after recovery |

The device-private provider journal also has `pending-publication-v1/` and
`provider-transaction.authority`; these never sync and cannot grant shared
graph authority.

Shared-provider paths and files may be owned by a different operating-system
user than the Tine process. This is normal for Android shared storage, NFS,
containers, and shared-group deployments. Unix UID equality is therefore not
an admission rule. Tine instead requires capability-relative no-follow opens,
the expected directory/regular-file kind, bounded names and sizes, immutable
content validation, and the protocol's exact descriptor/frontier relationships.

The clean shared descriptor names the immutable lazy-genesis baseline root,
its exact provider index, source capture and accepted manifest frontier directly. It contains no legacy
enrollment head, promotion proof, Patricia root, SQLite identity or persistent
projection-work state. The matching private `lazy-genesis.shared` record says
only which exact descriptor this device joined and whether it initiated or
joined the graph. Current semantic facts remain in disposable SQLite; durable
history remains the baseline plus manifest-committed operation tail.

### 1.2 Device-private app data

Local managed state is deliberately outside the graph. The Tauri shell derives
a private root for the exact graph and stores the following components there.
The Tauri binding selects the storage regime; inside the selected private
root, `lazy-genesis.marker` is the sole managed-authority commit marker. All
projection and query state may be reconstructed from the immutable baseline
and manifest tail.

| Path below the graph's private root | Writer | Reader | Format | Lifecycle |
| --- | --- | --- | --- | --- |
| `sparse-v2/binding.json` | Tauri explicit activation/join | ordinary startup selector | canonical JSON app binding v2 | durable local opt-in; deleted on Return to Direct Files |
| private enrollment `lazy-genesis.marker` | clean activation/join installation | production managed open | canonical activation marker v1 | written last; sole local managed-authority selector |
| private enrollment `lazy-genesis.shared` | clean share/join transition | clean runtime reopen | canonical clean descriptor digest plus local initiator/joiner role | device-local lifecycle fact; no semantic history or projection state |
| `sparse-v2-recovery/` | Tauri recovery/escape flow | Tauri recovery | renamed private component trees | temporary crash recovery |
| `archive/lazy-genesis/{manifest.postcard,commit.postcard,catalog.snapshot,segment-*.pack}` | clean activation | clean open/join | immutable baseline pack v4 plus commit v1 | authoritative baseline; installed before the marker and never mutated |
| `archive/operations/{lineage.claim,archive-instance-v1.claim,objects/,batches/}` | clean local/external/provider commit | causal replay and publication | content-addressed objects plus manifest-last batches | authoritative append-only tail after the baseline |
| `receipts/{projection-receipts.claim,projection-receipts.init,bases,intents,completions,attempts,forensics}/` | projector | recovery/readiness checks | projection store v5 and versioned rows | derived receipts and diagnostics |
| `receipts/.pending-cleanup/{round-0,round-1,round-robin.state}` and suffix authority files | receipt cleanup | receipt cleanup | bounded cleanup queue | disposable maintenance state |
| configured projection SQLite file and sidecars | clean runtime | managed queries/navigation and identity preflight | current `tine-storage` SQLite schema | disposable; missing/stale/corrupt state rebuilds from baseline plus manifests |
| application runtime `move-episodes/` | correlated multi-page operation | idempotent retry/reopen | immutable episode sidecars | retained only to bind an application retry to its manifest |
| device-private provider journal | clean shared publisher | interrupted provider publication | bounded publication/recovery records and lock | private transport recovery; never semantic authority |

Emergency return publishes the sibling app-private selector
`storage-mode-selections/<graph-digest>.direct-v1.json`. Ordinary startup checks
this before managed binding discovery, so retained managed bytes cannot
resurrect themselves. The receipt is retired only after an explicit fresh
managed activation has quarantined the former private root and published its
new binding.

The following path families are **retired pre-0.7 artifacts**, not an alternate
production layout: `archive/bootstrap-v1/`, `archive/engine-history/`,
`archive/promoted-runtime.state`, the block/name/path/UUID Patricia indexes,
`archive/projection-work-index-v1/`, `archive/reference-catalog-v2/`, the old
multi-record enrollment tree and reservation, `reconciliation/`, runtime
scratch, `managed-local-journal-v1/`, `local-authorship-v1/`,
`inactive-bootstrap-publication-v1/`, `inactive-shadow-projections-v1/`,
`migration-source-backups-v1/`, and `bootstrap-source-capture-v1/`. Production
open never treats any of them as authority. Their decoders/construction paths
remain only in the test oracle while the source is physically separated; a
real graph containing only this state is refused and can Return to Direct
Files for a fresh clean activation.

Temporary prefixes (`.tmp-`, `.head-tmp-`, `.record-tmp-`,
`.authority-tmp-`) and `.staging` files have no authority until their named
atomic publication completes. Unknown canonical-looking files are errors;
recognized provider temporary files mean “delivery may still be settling.”

### 1.3 Direct Files disposable graph projection

Direct Files stores one app-private
`direct-files-projections/<canonical-graph-path-digest>.sqlite` database outside
the graph. It contains only the same parser-derived physical page, block, task,
property, tag, and search facts accepted by managed storage's disposable
projection; it contains no binding, oplog frontier, sync role, or authority
stamp. Markdown/Org remains the sole Direct Files authority.

The existing parsed `PageEntry + Arc<Document>` cache feeds one background
SQLite owner. The database retains each page's exact caller-owned content
revision together with the Direct fact-extractor version as disposable adapter
metadata. Bumping that extractor version forces one background re-lowering when
unchanged source bytes acquire new physical facts. A full warm-cache installation
compares those revisions and lowers only changed or missing pages; a clean
reopen lowers none. One-page cache upserts and deletes enqueue coalesced page
deltas. The editor, watcher, and save paths never wait for SQL. Indexed reads
are admitted only when the worker has published the exact current parser-cache
generation. One app-private sidecar lease permits only one graph instance to
publish into a projection database at a time; a concurrent window or process
that cannot acquire it stays on the parser evaluator. This prevents an older
instance from replacing facts behind another instance's locally-ready
generation watermark. A missing, stale, corrupt, incompatible, leased, or
unwritable database
therefore uses the established parser evaluator and cannot block graph open,
save, or external file observation.

The switched read families are the conservative task-query subset already
accepted by `sparse_task_query_eligibility` (task markers plus priority,
scheduled/deadline and presentation directives), literal fuzzy-search candidate
selection (including the `((` picker), and the original-case referenced-page
inventory used by autocomplete and navigation. They also include page aliases
and real-page ownership, explicit backlink and safely tokenizable unlinked-
reference candidate selection, persisted/runtime block-identity lookup,
block-referrer candidates, and distinct-referrer counts. Once current, these families
enumerate SQLite task candidates and re-evaluate every returned raw block
through the existing parser query evaluator, or obtain a generation-bound
candidate/name set before applying the existing parser-owned matching and
presentation semantics. They no longer use manual whole-graph candidate scans
or second in-memory alias, reference-candidate, block-identity, referenced-name,
or block-ref-count semantic caches as their ordinary route.
The bounded generation-keyed
memo of already-shaped frontend result DTOs remains Tine-native: SQLite cannot
own parser AST semantics or presentation reuse, and dropping that memo would
turn reactive re-renders into repeated SQL plus parser evaluation. If SQLite is
unavailable, the same parser evaluator remains the correctness fallback; it is
not a second candidate index. Referenced-name fallback walks only the already-
parsed page cache and deliberately retains no separate semantic memo. Non-UUID
`id::` values and names that cannot be safely narrowed by SQLite tokenization
also use that parser fallback. All other query, navigation, and search families retain their existing
implementation until an equivalent generation-bound differential packet
replaces and deletes each old route.

## 2. Enrollment and synchronization state machine

### 2.1 Actors and authority

| Actor | Owns | Never owns |
| --- | --- | --- |
| Tauri selector | private binding and explicit Direct/Managed choice | oplog truth, enrollment history |
| local enrollment owner | local lifecycle record and its OS writer lease | another device's state, graph Markdown truth |
| managed actor (`SyncRuntimeHandle`) | admitted mutations, local journal drain, archive publication, projection scheduling | authority before a validated active enrollment |
| initiator | creation/publication of the shared descriptor; initiator enrollment transition | joiner's private state |
| joiner | its own local archive/enrollment after validating the exact shared descriptor and provider cut | rewriting the descriptor or adopting incomplete provider bytes |
| provider transport | durable copy/rename/retirement of exact files | semantic acceptance; directory presence is not enrollment |
| immutable oplog/archive | managed page/journal semantic truth | assets, PDF sidecars, config/settings |
| SQLite, scratch, projection work/receipts | acceleration, reconstruction, diagnostics | semantic truth or permission to overwrite Markdown |

Authority is transferred only by a validated, durably published record while
the current owner retains the relevant lease/capability. A path name, a newer
mtime, a cache row, or provider arrival alone never transfers authority. Any
operation that observes a changed generation/descriptor/frontier must restart
from that observation instead of completing under stale authority.

### 2.2 Local lifecycle

1. **Direct / absent** — no private binding; startup opens Direct Files and
   does not inspect shared bytes.
2. **ShadowImport** — explicit activation first remains Direct/absent while it
   reads the live source once into a sealed private capture and prepares an
   inactive immutable bootstrap from that capture only. One fresh complete
   live-source scan must then match the sealed capture. Only after that proof
   does Tine publish `ShadowImport`; a mismatch leaves durable enrollment
   absent, refuses without changing Direct Files, and permits a clean retry
   from the current Markdown/Org bytes. If the pre-enrollment reservation's
   source digest differs on retry, Tine preserves and detaches that attempt's
   archive, receipts, SQLite state, runtime, backup, preparation, enrollment,
   and reservation as one reconstructible diagnostic episode before rebuilding.
   The new sealed capture and the live graph are not moved. Archive detachment
   happens first and enrollment last, so an interrupted reset retains the old
   reservation and repeats safely. Active/shared enrollment is never retired.
   Bootstrap semantic lowering is size-adaptive. Canonical encoded operations
   remain in memory through partitioning and detached authoring while their
   measured retained bytes stay at or below 128 MiB; this ordinary route writes
   no operation spool and performs no operation external-merge sort. Crossing
   that byte budget deterministically spills the same canonical records into
   the bounded external-sort path. Both routes must produce byte-identical
   aggregate and commit records. The process-only terminal SQLite optimization
   retains only authenticated accepted events after authoring, never a second
   operation-spool artifact.
3. **VerifiedLocal** — bootstrap, backup, shadow projection, and SQLite proof
   agree. Authority is still inactive.
4. **LocalActive** — promotion publishes the accepted runtime state; the actor
   acquires enrollment/archive leases and becomes the sole managed writer.
5. **Blocked / incompatible / corrupt / ambiguous** — typed terminal or
   retryable evidence; no fallback writer is silently admitted.
6. **StoppedSafe / StoppedCrashed / Terminal** — clean drain publishes a safe
   handoff; crash recovery may resume its own unsafe state, adopt a safe
   handoff, or take over a crashed unsafe state after validation.

Activation diagnostics run in this order: source capture; bootstrap import
preparation; immutable install; backup proof; SQLite open/build; shadow byte
verification; promotion/authority confirmation; reconciliation baseline and
actor open. Progress reporting is observational and never creates a timeout
fallback.

### 2.3 Sharing lifecycle

1. **LocalActive → SharePrepared (initiator).** The initiator publishes every
   exact chunk of the sealed lazy-genesis baseline, publishes the small index
   that binds those chunks, publishes the accepted operation tail, and only
   then publishes the one shared descriptor and records the matching local
   phase.
2. **Direct/explicit join → Joining (joiner).** The joiner reads that exact
   descriptor, reconstructs the descriptor-bound baseline and causal
   manifest/object closure in a private staging area, and replays it to the
   advertised frontier. Before replacing local managed authority it compares
   the complete disk-expressible page/outline semantics with the currently
   synchronized Markdown/Org graph. A mismatch leaves both authorities
   unchanged; equality installs the provider history without rewriting graph
   bytes. Local-only endpoint and device identities remain local.
3. **SharePrepared/Joining → SharedActive.** Each device records its role
   (`Initiator` or `Joiner`) in its own enrollment. The descriptor remains the
   shared identity; local endpoint/device IDs remain local.
4. **SharedActive operation.** A local edit is durably journaled, authored into
   the oplog, accepted locally, projected, then published as objects → intent →
   manifest/recovery copy → covering frontier head. Peers admit only complete,
   validated batches and apply them in causal order. A peer renders accepted
   semantics against its own exact current bytes, using the source operation's
   authenticated render-base block identities. It therefore retains harmless
   receiver-local representation such as CRLF while preserving parser-owned
   structural layout, including non-bulleted Markdown headings whose content
   changed. The source endpoint's target bytes do not become receiver write
   authority.
5. **Interrupted transfer.** Missing/temporary/reordered bytes remain pending;
   exact immutable collisions or inconsistent stable cuts block. A retry
   resumes from durable observations rather than inventing state.

On every cold installation of a `SharedActive` actor, the first production
watcher turn performs one imprecise provider scan before relying on exact
filesystem callbacks. Provider bytes may have arrived while Tine was stopped,
before an inotify watch existed; graph-local text scanning alone cannot prove
that shared transport is current. Local-only managed storage never performs
this provider adoption merely because another device's namespace is present.

Provider traversal and incomplete projection recovery may span several actor
turns. `Recovering` reports bounded progress only; it is not itself a content
notification. When an inbound provider batch becomes visible in SQLite and the
receiver's Markdown/Org projection, the actor emits one `ProviderMutation`
tick naming that batch. If an interleaved serialized application request
finishes a retained provider projection between watcher ticks, the actor keeps
that batch identity until the next tick emits the same notification; a read
must not consume the live-view wake-up. The production watcher treats that tick
as an observable graph change and schedules a continuation for any remaining
provider work. A terminal quiet watcher admission remains `AdmittedNoop` and
must not manufacture a frontend refresh or conflict.

**Known provider work is itself a runnable work source.** Delivered provider
evidence is work this device never performed; it arrives as bytes another device
wrote, and once the delivery is over no further filesystem event announces it.
One actor turn settles one lane, so a turn that consumed a watcher epoch reports
`Admitted*` while delivered provider evidence is still retained — that report is
not evidence of a quiet graph. The actor therefore publishes
`SyncRuntimeStatusSnapshot::provider_runnable`, the exact predicate `tick`
consults before routing into the provider lane, and the production watcher
schedules its next turn while `has_runnable_work()` (a pending watcher epoch OR
runnable provider work) is true. `provider_pending` is NOT that predicate: it is
a broad protocol inventory which also counts durable publication intents that
legitimately remain after publication, so a scheduler driven by it would never
sleep. Conversely, when `has_runnable_work()` is false the scheduler arms no
timer at all and blocks on the kernel; and a turn that reports `Idle` while
still naming runnable provider work — the ready queue blocked on causal
dependencies whose bytes have not been delivered — is paced by the ordinary
retry backoff rather than the progress cadence, so a blocked dependency cannot
become a poll loop.

**A causal dependency is never queued behind its dependent.** The direct
provider manifest lane advances only its front entry. A batch whose dependency
sits behind it in that same queue therefore deadlocks the pair: the front batch
re-inspects the dependency every turn while the dependency never reaches the
front, and — by the rule above — no later filesystem event breaks the tie, so a
peer's edit is stranded while the actor honestly reports runnable work forever.
Membership in the lane's dedupe set does not by itself mean a dependency will be
admitted in time; position is part of the contract. Whenever admission blocks on
a dependency, that dependency is moved to the front of the lane (restoring the
deque/set invariant if they disagree). Queue order otherwise follows provider
scan order, which is why this state reproduces only intermittently in journeys —
`a_dependency_queued_behind_its_dependent_is_promoted_ahead_of_it` pins the rule
deterministically.

### 2.4 Lazy activation and clean runtime boundary

The accepted next activation generation has one authority-changing record:
**one final lazy-genesis authority marker**. The Tauri binding records opt-in
intent and permits setup/resume UI, but it is not semantic authority. Until the
final marker exists, Direct Files remains the sole authority and every baseline,
SQLite, receipt, and episode artifact is disposable.

The marker binds exactly the workspace, lineage, immutable baseline root,
sealed source-capture description, accepted-frontier digest, and watcher fence.
SQLite identity is deliberately absent: SQLite is a frontier-stamped disposable
projection and can be rebuilt without changing the marker or semantic truth.
The marker is published only after the baseline is durable and one final
byte/inventory comparison under the watcher fence matches the sealed source.

Each page capsule carries the exact original Markdown/Org bytes once, one
deterministic CRDT checkpoint constructed directly from its terminal page
state, plus its compact causal dependencies.
One canonical activation-record pass fans each parsed page into both the
baseline pack and bounded SQLite materialization chunks. Neither candidate is
published by that construction pass, and SQLite does not re-read, re-parse, or
replay the graph to derive the same terminal state a second time.
The single catalog checkpoint is constructed by the same direct terminal-state
builder, and the sealed manifest binds its non-derivable catalog document ID.
These checkpoints are baseline semantic/causal state, not fabricated interactive
history: their construction authors no `SemanticOperation`, batch, ordinary
mutation receipt, partition, or detached bootstrap part. Untouched page
checkpoints remain unopened in the lazy pack until a page read or first ordinary
operation needs one.

Reading one baseline page costs that page, not the pack. A sealed segment pack
is written once and never rewritten, so its whole-pack digest is proved against
the sealed manifest at most once per opened baseline — re-proving it per page
would make every consumer that walks the graph, including the clean watcher's
full scan, cost `O(pages x segment bytes)`. The proof is discarded and repeated
if the pack is relocated. Each page still verifies its own capsule bytes against
the sealed descriptor digest on every read, so damage to the bytes a caller
actually receives is rejected regardless of that retained whole-pack proof
(`lazy_genesis::tests::lazy_genesis_proves_each_sealed_segment_at_most_once`
and its damage siblings).

The corresponding crash states are exhaustive:

| Durable state at restart | Authority | Required behavior |
| --- | --- | --- |
| No final marker; no episode | Direct Files | Start activation from the current graph. |
| No final marker; partial baseline or SQLite | Direct Files | Ignore/quarantine the episode and rebuild from the current graph. |
| Final comparison differs; no marker | Direct Files | Preserve bounded diagnostics and restart from a fresh source observation. |
| Marker publication began but no complete canonical marker exists | Direct Files | Treat every candidate artifact as uncommitted. |
| Valid marker and complete baseline | Managed baseline plus later accepted operations | Open the lazy engine; open matching SQLite or rebuild it. |
| Marker exists but baseline validation fails | No silent writer | Refuse managed admission and offer recovery or Return to Direct Files. |
| First materialization has no durable ordinary operation | Baseline page capsule | Discard the partial materialization and retry deterministically. |
| First ordinary operation is durable | Baseline plus that operation | The ordinary document state supersedes the page capsule. |

Managed mutation ordering is likewise fixed before native identity-index
removal: validate SQLite at accepted frontier `F`, prepare the semantic
operation and exact row delta, durably append the operation as `F+1`, then
commit SQLite at `F+1`. A crash after the durable append leaves a stale
projection which is replayed/rebuilt. A SQLite failure after the append does
not turn the accepted edit into a retryable save or permit a duplicate write.
SQLite must never publish `F+1` before semantic history does.

Application protocols which need retry-stable identity, currently cross-page
subtree moves, supply one deterministic `BatchId` to the same clean commit
pipeline. Their immutable episode record and manifest fingerprint are
published before the operation manifest. A failed episode publication cannot
reach the manifest; after the manifest exists, cold replay plus that record
turns a repeated request into one recovered result rather than a second edit.

An unrelated accepted batch may advance a page's causal frontier without
changing its rendered bytes. In that case the latest clean projection manifest
remains valid predecessor evidence only after the projection planner replays
the current semantic page and proves equality except for that frontier. Exact
bytes, path, page identity, claims and layout annotations must still match.

A receiver-local projection can legitimately differ byte-for-byte from the
source target while expressing the same accepted semantic page. On a later
local edit, the current accepted manifest head remains the semantic authority,
but not an assertion that the receiver copied its target bytes. Capture must
reprove the receiver's live Markdown/Org as an exact source for that accepted
page and bind the resulting annotations and bytes to the current manifest
head. A semantic mismatch enters external reconciliation; it may not be hidden
by canonical rendering or bypassed by trusting live bytes alone. This proof is
reconstructed from the manifest plus live file on reopen and therefore does
not require another persistent page/layout index.

For a valid clean marker, the immutable baseline plus committed ordinary
manifests is the complete semantic authority. The runtime reconstructs any
current projection-head map in process memory from those manifests; it neither
opens nor updates a persistent projection-work index. SQLite owns current exact
path identity and current canonical page-name identity. External reconciliation
reads both affected path owners and name-acquisition candidates from the
frontier-matched SQLite projection, reproves the corresponding baseline or
latest-manifest bytes against the engine, and then uses the same structural
page/block matcher as the established importer. It must not ask a native
Patricia path or page-name index to duplicate SQLite ownership. A content or
path-only edit of an existing physical same-name page does not reacquire its
logical name; only a creation or exact-title change enters name-acquisition
preflight.

One canonical page name has one owner, and a graph may legitimately hold more
than one physical file for it. Activation already resolves that: it selects one
authoritative source per canonical page name and per portable path in exact-path
order and retains every other file untouched, with no page of its own
(`bootstrap_authoritative_source_paths`). External reconciliation makes the SAME
selection. A source that carries no accepted page identity and whose decoded name
is already owned — by an established page, or by an earlier exact path in the
same transaction — acquires no identity: no page is created for it, no operation
touches it, and its exact bytes are still observed by the transaction. A clean
requested set likewise selects the first exact path per portable identity rather
than refusing the set. Refusing instead turned an ordinary duplicate into a
permanent graph-wide denial: planning failed for every affected path, on every
tick, with no user action that could clear it. An accepted page is never
withdrawn this way — it keeps the identity it already has, and a real title
change into a name another page owns remains the visible ambiguity preflight
refuses.

An exact watcher callback queues only the named managed paths. An imprecise
callback, and every cold open of a clean marker, queues one full comparison of
current Markdown/Org paths, SQLite paths, and released paths named by accepted
manifests. Equal bytes acknowledge the watcher epoch without an operation;
changed, created, deleted, and jointly observed renamed paths become one
external-reconciliation operation. A manifest-committed operation whose SQLite
or Markdown derivative is interrupted retains one affine continuation. The
watcher epoch remains unacknowledged until that continuation and any
observations queued behind it are reconciled, and clean shutdown drains this
work before reporting `StoppedSafe`.

SQLite schema 20 provides the physical replacement for all four native
identity-index families. Page-name and portable-path rows contain one complete,
application-owned causal point record; exact names and paths are inline and do
not depend on a content-addressed side blob. External Logseq UUID introductions
and block-home claims are append-only bounded histories which preserve every
claimant. Every causal origin is explicitly either `Baseline` or an accepted
`(batch, dot)`; activation never fabricates a bootstrap batch merely to seed an
index. The old Patricia values remain only as a differential oracle until the
single production cutover, and are then deleted rather than retained as a
second ready route.

The clean engine does not hydrate those baseline UUID introductions into a
resident identity map. During ordinary operation the exact-frontier SQLite
projection supplies bounded baseline candidates, the engine unions them with
post-baseline introductions from committed manifests, and current CRDT block
state decides whether a candidate is live and unique. If disposable SQLite is
missing or corrupt, terminal reconstruction derives one rebuild-scoped
candidate snapshot from the immutable lazy-genesis capsules, including every
ambiguous claimant, and drops it when SQLite publication finishes. That
snapshot is a construction input, not a runtime index or semantic authority;
ambiguous baseline claims remain unresolved after reconstruction.

## 3. Invariants and versioning

1. The threat is crash, power loss, torn write, and interrupted/reordered file
   sync—not a malicious byte-forging actor. Content digests detect accidental
   damage and name immutable content; they are not a security authenticator.
   The sole `hmac::verify` call remains only for frozen legacy enrollment
   history compatibility.
2. The immutable oplog is the source of truth for managed page/journal content,
   IDs, names/paths, references, and properties. Markdown is a projection when
   managed mode is active. Assets, PDF sidecars, `config.edn`, and app settings
   retain their separate authorities.
3. SQLite, reconciliation databases, scratch, Patricia lookup indexes,
   projection-work indexes, and transient receipts are disposable. Deleting or
   version-mismatching one may cause exactly one bounded rebuild, never a second
   rebuild on the following open. A complete rebuild must be linear in graph
   size and finish within 10 seconds on the release corpus.
4. Authoritative bytes are append-only or atomically replaced under an exact
   observed-generation/lease check. A cache cannot authorize oplog mutation or
   Markdown overwrite.
5. Shared publication is closed: a manifest names its complete object set, and
   a frontier head may advertise it only after all prerequisites and recovery
   evidence are durable.
6. A joiner must be able to reconstruct all device-private state from a
   complete shared archive. Local app data is not synchronized and must not be
   required from the initiator.
7. Simulator/test code may import production wire/storage code. Production may
   not import the `simulator` compatibility module.
8. Direct Files remains isolated: no passive `.tine-sync` discovery, managed
   recovery, oplog write, or managed cache work occurs without the validated
   private binding or an explicit activation/join command. Its separate
   app-private graph-fact projection contains no managed state and grants no
   authority.

### 3.1 Refusal scenarios

Every public durable refusal must carry one of these stable scenario IDs. An
internal fail-closed validation is classified when it reaches the public
open/activation boundary; it does not need to duplicate the identifier at
every decoder call site. A transient condition that is safe to retry is not a
durable refusal; a disposable cache failure must rebuild instead of appearing
in this table.

| Scenario ID | In-scope failure | Required response |
| --- | --- | --- |
| `MS-REF-CRASH-TRUNCATED` | Crash, power loss, or interrupted provider delivery leaves a canonical record or immutable object truncated/incomplete | Preserve authoritative bytes; retry if delivery may still be settling, otherwise diagnose the exact corrupt component |
| `MS-REF-DISK-CORRUPT` | Disk/media error changes an immutable digest-addressed record or authoritative lifecycle record | Refuse the affected authority transition; retain recovery evidence and identify the component |
| `MS-REF-SYNC-CONFLICT` | A file-sync provider supplies conflicting bytes for the same immutable identity or a provider cut changes during admission | Do not choose bytes by mtime; retry a moving cut or block a stable immutable collision |
| `MS-REF-CONCURRENT-WRITER` | Another honest Tine process holds the exact enrollment/archive/SQLite OS lease | Refuse the second writer while the lock is held; reopening after release must work |
| `MS-REF-STALE-GENERATION` | An honest concurrent operation advances a binding, frontier, lease identity, or generation after validation began | Abort/retry the stale operation; never publish under the superseded observation |
| `MS-REF-UNSAFE-FS-KIND` | Sync delivery, filesystem damage, or an external tool replaces an expected directory/regular file with a symlink, special file, reparse point, or unexpected hard-link alias | Refuse access through the substituted entry without following it |
| `MS-REF-MALFORMED-IMPORT` | Imported/shared Markdown, Org, descriptor, manifest, or operation bytes cannot be decoded within declared bounds | Leave source/authoritative history unchanged and report the bounded invalid component |
| `MS-REF-BOUNDS` | Honest corruption or malformed imported/provider input exceeds explicit memory, depth, count, or byte bounds | Reject before unbounded allocation or traversal and report the bounded class |
| `MS-REF-PROTOCOL-INCOMPATIBLE` | An honest device or restored graph supplies a recognized managed-storage component whose schema/protocol is newer or otherwise incompatible with this build | Preserve the component unchanged, refuse interpretation, and identify the component so the user can upgrade or rebuild from Direct files |

Every public durable open/activation refusal carries its scenario ID separately
from its bounded reason/stage code. Retryable open failures do not invent a
scenario; if a lower storage boundary detects a durable refusal it emits the
literal table ID and the public boundary preserves it.

A managed application/editor refusal must also be *attributable*. An
internal-invariant refusal — one that is not a caller error and has no user
remedy — still names the stage it came from, and an error crossing between the
editor and application surfaces preserves that stage instead of collapsing it.
The reason is a boundary, not a preference: on a platform Tine cannot debug
interactively (Android instrumentation, a user's device, a bug report) the
returned value is the only evidence that exists, and an unattributed
`ActorRefused` makes the failure permanently undiagnosable. An editor rejection
of a request the *application layer itself* constructed is such a refusal: it
is never reported as an invalid caller request, and it is never anonymous.

Attributability is now total rather than aspirational: no call site on the
managed application or editor surface may construct the payload-less
`SyncApplicationPageRequestError::ActorRefused` /
`SyncEditorRequestError::ActorRefused`. Those variants survive only as the
declaration, their two `Display` arms, and the total mappers that re-shape an
already-decided refusal when it crosses between the two surfaces; every origin
uses `ActorRefusedAt`, `ActorRefusedAtWithCode`, or
`ActorRefusedAtWithDebugDetail`. The rule is mechanical, not editorial:
`sync_runtime::tests::managed_save_refusals_cannot_be_constructed_without_a_site_name`
reads the production source and fails on any new bare construction, and on a
collapse of the named-stage inventory. This closes the gap that left the
Android post-activation save reporting `debug_detail="none"` with no stage — a
refusal that could have come from any of 131 unnamed sites.

### 3.2 Clean-runtime save settlement

The clean baseline-plus-manifest runtime and the retired legacy coordinator are
two **distinct** retained-publication state machines, and a request may never be
routed from one into the other.

A clean local mutation that reaches its manifest commit and then fails to apply
disposable derived state (SQLite and/or exact Markdown projection) returns
`CleanActorMutationOutcome::DurablePending` and retains an affine continuation
in `CleanRuntimeActorCore::pending`. That continuation is advanced only by
`retry_pending`. The legacy coordinator's `PendingLocalMutation::Published`
continuation is a different object that the clean actor never writes.

Therefore, when the clean runtime is installed, an application save that lands in
`DurablePending` settles through the clean actor, bounded by
`MAX_EDITOR_SETTLE_TURNS`. Exactly two outcomes are permitted:

- the retained continuation settles, and the request reports **applied/saved**;
- it does not settle within the budget, and the request reports
  **`Deferred { RetryableRetainedPublication }`**.

A **refusal is forbidden here**, and has no entry in the §3.1 table, because it
would defend against no in-scope failure: the manifest commit is already
durable, and the outstanding work is disposable derived state whose contract is
recovery, not refusal (G2). A refusal with no in-scope scenario is an
availability bug. This is not hypothetical — routing the clean outcome into the
legacy settlement returned `ActorRefusedAt("require_pending_publication_absent")`
for *every* clean-runtime save, which is why Android managed saves never worked.

Two further rules keep the settlement honest:

- A retained continuation belonging to an **earlier** batch is reported as
  `CleanActorMutationOutcome::RetainedPriorPending`, never as the caller's own
  `DurablePending`. That submission never executed, so settling the earlier
  batch must defer the request rather than report it saved with the page's old
  bytes.
- The failure that caused the retention is reported separately from the save's
  own outcome (`SyncRuntimeHandle::last_retained_publication`). A converged
  retry produces an ordinary successful save while the underlying cause still
  costs a retry on every write; the Android instrumentation receipt carries this
  report on both the success and the failure path so that cause stays visible.

The structural claim — a clean runtime never reaches the legacy publication
settlement — is enforced by tests
(`clean_runtime_application_save_settles_its_own_retained_publication`,
`clean_runtime_application_save_defers_when_retained_publication_cannot_settle`),
which assert the actor's legacy-settlement counter stays at zero, not by this
paragraph.

The settlement budget is an upper bound, not a target. A retry that reproduces
the **same phase and the same failure detail** as the previous turn has made no
progress against a deterministic failure, so the loop stops at that second
identical observation and defers once. Spending all `MAX_EDITOR_SETTLE_TURNS`
turns on a permanent failure buys no chance of settling and charges the whole
cost to the user's save. Any change of phase or detail counts as progress and
keeps the loop running to the budget.

### 2.10a Durability barriers by artifact class

Platform durability policy is stated **per artifact class**, never globally.
`crate::filesystem_durability::DurabilityArtifactClass` names the two classes,
and every projection directory barrier passes through it:

| Class | What it covers | Policy |
| --- | --- | --- |
| `PrivateDurableAuthority` | The oplog manifest, object archive, local journal and receipt store below app-private storage — and graph-tree artifacts the graph is the **sole** authority for: conflict copies, trash, withdrawn bytes, assets. | Strict on **every** platform, Android included. A barrier the filesystem refuses is a real durability failure. |
| `SharedReconstructibleProjection` | The Markdown/Org projection of an already-accepted manifest into the user's graph tree. | Strict everywhere except Android. On Android only, and only for `PermissionDenied`/`Unsupported`/`InvalidInput` (`EPERM`/`ENOTSUP`/`EINVAL`), the barrier **degrades**. Every other errno stays fatal. |

The crash story for the degraded case still holds: the projection is derived
state. The accepted manifest in app-private storage — which keeps its strict
barriers — already records those bytes, and a crash that loses an unflushed
directory entry is repaired by the projection drain on the next open, by the
same mechanism that finishes an interrupted projection. Retrying a capability
refusal cannot ever succeed, so retrying it forever is not crash-safety; it is
an availability bug that strands the user's edit, which is exactly what Android
CI run 32088229039 recorded (`phase:ProjectionDrain`,
`detail:Invalid argument (os error 22)`, `settled:false`, 64 turns).

Because the device is the only oracle for these semantics, every platform
primitive on the projection leg — the directory flush, `renameat2` with
`RENAME_NOREPLACE`, projection file `fsync`, and the no-follow `openat` of a
projection parent or file — names its operation and its location in the error it
returns, and `execute_manifested_projection_work` prefixes the page path. A
device receipt therefore reads `projecting "pages/X.md": fsync of the projection
parent directory failed at chain depth 2/2 (…): Invalid argument (os error 22)`
rather than a bare errno. `ErrorKind` is preserved, because guarded-conflict
classification and the durability policy both match on it.

The class split is enforced by tests, not by this table:
`filesystem_durability::tests::only_the_reconstructible_projection_class_degrades_and_only_on_android`
and `model::tests::only_the_reconstructible_projection_barrier_degrades_on_android`
at the primitive, and
`sync_runtime::tests::clean_runtime_save_survives_an_android_projection_directory_barrier_refusal`
plus `…::a_projection_directory_barrier_refusal_still_fails_closed_off_android`
at the save boundary.

### 2.10b No-clobber publication when the filesystem has no rename flags

The directory barrier is not the only primitive Android shared storage refuses.
Android CI run 32091898520 recorded the **flagged rename itself** failing:

```
retained_publication=… phase:ProjectionDrain settled:false turns:2
detail:projecting "pages/Smoke.md": renameat2(RENAME_NOREPLACE) publishing the
projection failed at "Smoke.md" -> ".Smoke.md.49a4ed18…"
```

with `Invalid argument (os error 22)` underneath. Two earlier lanes eliminated
this call by reading AOSP `FuseDaemon.cpp`, whose `do_rename` accepts exactly
that flag. The device disagreed. `RENAME_NOREPLACE` has to be provided by every
layer — the kernel FUSE client, the daemon, and the filesystem underneath it —
and on this path it is not. **Upstream source is evidence about upstream intent,
not proof about the running device; the receipt wins.** The same `EINVAL` is
reachable off Android on any filesystem without `rename2` flags (FAT/exFAT
removable media, some FUSE and network mounts).

`model::rename_projection_noreplace_with_class` therefore applies a capability
policy to the no-clobber publication, keyed on the same
`DurabilityArtifactClass`:

| Class | Policy for the flagged rename |
| --- | --- |
| `PrivateDurableAuthority` | The platform primitive and nothing else, on every platform. There is no second copy to rebuild these bytes from, so a non-atomic publication could leave a reserved-but-empty file at a live graph name after a crash. A filesystem that cannot provide the primitive fails the write. |
| `SharedReconstructibleProjection` | `EINVAL`, `ENOSYS` and `EOPNOTSUPP`/`ENOTSUP` from the flagged rename — and **only** those three, matched on the raw `errno`, not on `ErrorKind` — are read as "this filesystem does not implement that flag" and retried through the reservation fallback below. Every other errno (`EIO`, `ENOSPC`, `EACCES`, `EXDEV`, `EEXIST`, `ENOENT`) describes the operation rather than the flag and stays fatal. |

Unlike the directory barrier in §2.10a, this policy is **not gated on Android**.
The barrier policy gives a guarantee up, so it is confined to the platform that
forces the choice; this one keeps its guarantee and gives up only atomicity, and
failing the write closed on a FAT stick would be an availability bug with no
in-scope threat behind it.

**The fallback (`model::reserve_and_rename_projection`).** Reserve the
destination name with an exclusive create (`O_CREAT|O_EXCL`), then perform a
plain `rename` onto the reservation.

* *What it keeps.* Never silently destroy a file already at the destination —
  the one guarantee `RENAME_NOREPLACE` was there to provide. An occupied
  destination fails the reservation before anything has moved, and is reported
  as `AlreadyExists`, the exact error the flagged rename raises, so every
  guarded-conflict caller above is unchanged. A failed reservation is fatal, never
  a silent overwrite. If the plain rename then fails, the reservation is rolled
  back — but only when the destination is still, by physical identity, the
  placeholder that call created — so a failed publication leaves no zero-length
  file at a live page name.
* *What it gives up.* Atomicity of the name transition. Inside the window
  between the reservation and the rename the destination exists as a zero-length
  file, so (a) a crash there leaves a zero-length name, which the projection
  drain rebuilds from the accepted manifest on the next open exactly as it
  rebuilds any interrupted projection, and (b) an external writer that replaces
  the placeholder inside that window is overwritten rather than winning the race.
  Both are why the fallback is confined to the reconstructible class.

The answer is a property of the mounted filesystem, so it is remembered per
`st_dev` after the first capability refusal instead of costing a failed syscall
on every publication. The memo is consulted only for the reconstructible class
and is never load-bearing: an unknown device simply attempts the flagged rename
and learns from it.

The reconstructible-projection call sites are the ones bracketed by
`preflight_reconstructible_projection_chain` /
`sync_reconstructible_projection_chain`: retiring a live page to its attempt
recovery name, publishing the staged bytes onto the live name, withdrawing an
unsafe publication, restoring a displaced target, retiring a recovery artifact
into quarantine, and preserving a changed recovery artifact as a projection
conflict. The graph-tree write paths that are *not* on that leg —
`managed_atomic_create_with_proof`, `managed_atomic_write_with_conflict`,
`managed_atomic_replace_bound`, `managed_move_noreplace` — keep the strict
class, because in Direct Files the graph tree is the sole authority for those
bytes.

Enforced by `model::tests::only_the_reconstructible_projection_rename_falls_back_when_the_flag_is_unsupported`,
`…::the_projection_rename_fallback_refuses_an_occupied_destination_rather_than_clobbering_it`,
`…::a_projection_rename_fallback_that_cannot_complete_leaves_no_empty_destination`,
and at the save boundary by
`sync_runtime::tests::clean_runtime_save_survives_a_projection_rename_capability_refusal`
(which also compares the displacement artifacts against an uninjected control
save) plus `…::a_non_capability_errno_from_the_projection_rename_still_fails_closed`.

### 2.10c The shared-provider tree without rename flags, including the exchange

§2.10b left the shared-provider transport (`oplog/wire.rs`) alone as a
follow-up. Android CI run 32094662514 turned that into the next failure: the
managed save landed for the first time and the journey stopped one step later at

```
AssertionError: prepare shared failed: sync actor refused request:
scenario filesystem operation failed: Invalid argument (os error 22)
```

The flagged renames in that module operate under `<graph>/.tine-sync/v2/shared`,
and one of them — quarantining a publication's own abandoned staging entry — is
on the **happy path of every provider `Put`**. On a filesystem without `rename2`
flags, no object can be published at all, so share preparation cannot start.

The six shared call sites are classified individually. The two remaining flagged
renames in the same module belong to the **private** retry journal
(`ProviderRetryJournal`, outside the graph) and keep the strict class untouched.

| Site | Artifact | Class | Policy |
| --- | --- | --- | --- |
| `quarantine_unowned_staging` → `removed/orphan-<op>-<gen>` | An abandoned staging copy of bytes whose authority is the private retry-journal blob; the caller deletes this diagnostic again as soon as its identity matches. | Reconstructible | Reservation fallback (§2.10b). |
| `quarantine_provider_name` → `removed/<prefix>-<digest>` | FOREIGN bytes that took a name this device expected to own, preserved for forensics before the operation refuses. | Reconstructible — *only because the fallback keeps the no-clobber guarantee*: an occupied destination fails the exclusive reservation before anything moves, and a rename that then fails leaves the foreign file exactly where it was. | Reservation fallback. |
| `preserve_retirement_race` → `rename-evidence/retirement-race-<digest>` | The same, for a name re-created during a retirement. | Reconstructible, same argument. | Reservation fallback. |
| `reconcile_provider_retirement`, the `RENAME_EXCHANGE` | The validated original moving to its diagnostic name, swapping with this operation's journaled placeholder. | Reconstructible; see below. | Single placeholder-consuming rename. |
| `reconcile_provider_retirement`, the rollback exchange | Undoing the above when post-validation fails. | — | Attempted **only** while this device still holds the placeholder at the source name, i.e. only on the exchange path. After the fallback there is nothing to swap back and the refusal says so rather than pretending. |
| `reconcile_provider_retirement` → `rename-evidence/retire-placeholder-<op>` | The displaced zero-length placeholder moving to private evidence. | Sole authority of the exchange invariant | **Strict, no fallback.** This step exists only on the exchange path, and a filesystem without `RENAME_NOREPLACE` has no `RENAME_EXCHANGE` either, so the exchange fallback has already made it unreachable there. A filesystem that somehow provided one and not the other gets an honest named refusal rather than a two-step substitute whose crash window recovery cannot read. |

**The exchange decision.** An atomic swap has no no-clobber-shaped substitute, so
the reservation fallback does not apply. A three-step rename through a scratch
name was rejected: it introduces a window in which the retired bytes exist at
neither name, and a second window whose leftover the recovery path cannot tell
from a racing delivery.

What retirement actually needs is narrower than a swap. Before the exchange the
diagnostic name is already occupied by a **zero-length placeholder this operation
created**, whose physical identity was made durable in the private journal
(`staging_identity`) before anything moved. So the fallback is a **single plain
`rename(2)` of the validated original onto that placeholder** — atomic on every
POSIX filesystem, no scratch name, no third step. Its end state is exactly the
state the exchange path reaches one step later: source name gone, original at the
diagnostic name, placeholder inode unlinked.

*Crash windows.* There are exactly two, because it is one rename:

* **Before it.** The original is still at its name and the placeholder still
  holds the diagnostic name — byte-for-byte the state the reconciler starts from.
  Recovery re-enters the same branch and retries. No residue.
* **After it.** The source name is free and the diagnostic name holds the
  original. Recovery takes the "source absent" branch, validates the retired copy
  against the recorded identity, digest and length, and completes. If the rename
  was not yet durable in the parent directory the state is the first window
  again, which also converges.

There is no third window: `rename(2)` over an existing destination is atomic, so
the retired bytes are never absent from both names.

*What it gives up.* The exchange's **other** guarantee — that the source name is
never free. After the fallback the source name is free, so an honest concurrent
instance or a sync-service delivery can re-create it, and the placeholder is no
longer available as proof that the transition happened. Recovery therefore keys
on the diagnostic name holding the recorded original identity, digest and length;
anything found at the source name afterwards is treated as a racing replacement,
preserved as `rename-evidence/retirement-race-…`, and the operation refuses —
the same terminal shape the exchange path produces for a race, and strictly
better than the flat refusal the previous code gave in that state.

*Inode reuse.* Because the fallback unlinks the placeholder, a filesystem is free
to hand its inode number to the next file created at the freed source name, and a
racing delivery would then match the recorded `staging_identity` exactly.
"Is this the placeholder?" therefore requires **zero length as well as identity**.
That cannot exclude the real placeholder, which is always empty, and a
zero-length impostor that still slips through costs zero bytes.

**Reservation residue in the diagnostic namespaces.** The reservation fallback
publishes in two steps, so a crash between them leaves a zero-length file at a
deterministic diagnostic name with the source still in place — and every one of
these sites refuses an occupied destination, which would refuse that operation
forever. `removed/` and `rename-evidence/` are diagnostic-residue namespaces and a
zero-length entry in one of them holds no bytes to lose, so an EMPTY occupant is
reclaimed and the next attempt converges. A NON-EMPTY occupant is still reported
as occupied and left untouched: that is either a real quarantine copy or a file a
sync service delivered, and neither may be destroyed.

**Receipts.** Every refusal in this module names its primitive and both names —
`renameat2(RENAME_NOREPLACE) quarantining abandoned shared provider staging
failed at "publish-859b1a…-0" -> "orphan-859b1a…-0": Invalid argument (os error
22)` — because Android CI returns only a string and a bare errno costs a
~20-minute round trip to localise.

Enforced by
`oplog::wire::tests::shared_provider_publication_without_rename2_flags_matches_the_flagged_end_state`
and `…::shared_provider_retirement_without_rename2_flags_reaches_the_exchange_end_state`
(both compare the whole provider tree against an uninjected control run on the
deterministic simulator), `…::shared_provider_retirement_fallback_crash_windows_converge`
(both windows converge to the uncrashed state, and the two boundaries that belong
to the exchange path alone are asserted unreachable),
`…::shared_provider_retirement_fallback_preserves_a_race_at_the_freed_source_name`,
`…::a_non_capability_errno_from_a_shared_provider_rename_still_fails_closed`, and
at the sharing boundary by
`sync_runtime::tests::clean_share_preparation_survives_a_shared_provider_rename_capability_refusal`
(share preparation completes, the provider tree has the control run's shape with
no residue, and a peer joins from it) plus
`…::a_non_capability_errno_from_a_shared_provider_rename_still_refuses_share_preparation`.

Unix UID equality and “only the current user may write this path” are
deliberately absent. The threat model does not defend against a malicious actor
who can already rewrite the user's private filesystem, and those checks reject
honest Android shared storage, NFS, restored backups, containers, and shared
groups. Capability-relative no-follow access, exact file identity, link count,
OS locks, digests, generations, and decoders carry the in-scope invariants.

Android bootstrap durability likewise does not require the application sandbox
to authorize a filesystem-wide `syncfs` operation. Source capture, prepared
bootstrap state, and migration-backup proof share one policy: Tine uses
`syncfs` as an optimization where the device permits it; a permission,
unsupported, or invalid-operation response falls back to synchronizing every
regular file and directory in that exact app-private tree. Other I/O failures
still abort activation, and the graph projection remains untouched until the
private state has been sealed.

Android app-private projection receipts retain create-new temporary-file
writes, exact-byte collision checks, file-content synchronization, and atomic
rename publication. Directory creation and immutable publication stay on
ordinary `mkdirat`/`openat`/`renameat` primitives throughout; opening the root
through Android's ordinary API and then re-entering cap-std preflights for its
children would reproduce the same false permission refusal one level down.
They do not require the hard-link-based no-replace primitive used by the generic
publisher: some Android app filesystems deny hard links even though ordinary
app-private create, write, sync, and rename operations are available. Receipt
directories are likewise opened through ordinary app-private directory handles
and classified from the retained handle, rather than requiring a preliminary
`fstatat` check or the Linux hostile-replacement `O_NOFOLLOW` primitive.
Receipt files follow the same rule: ordinary app-private open, then retained
handle type, length, and bounded-byte validation. This applies to the receipt
root as well: Android does not have to accept creation relative to a Linux
capability-style parent handle when its ordinary app-private file API is
available. Honest concurrent Tine writers remain excluded by the runtime lease;
a hostile process inside the same application sandbox is outside this threat
model.

Before an enrollment binding exists, projection receipts are reconstructible
bootstrap state rather than authority. If Android cannot reopen a receipt tree
left by an interrupted or older activation, retry retains one sibling
`receipts.pre-promotion-failed` diagnostic tree and initializes a clean receipt
store from the unchanged Markdown/Org source. Once enrollment has promoted the
receipt-store identity, this recovery is forbidden: normal exact identity and
receipt recovery rules apply.

The same rule governs the archive a clean activation builds. Before the
activation marker is committed, the archive carries no authority and is
reconstructible from current Direct Files, and the clean lane records no private
activation reservation that a later attempt could use to attribute it. An
attempt that refuses before that marker therefore retracts the archive it
created, and only that one: an archive that predates the attempt is left exactly
where it is, so genuinely foreign residue is still refused as
`AmbiguousOrForeignResidue { ArchiveResidue, SyncConflict }`. Without the
retraction, one ordinary external write landing during activation — which makes
the final source proof refuse `Retryable { durable_stage: Absent }` — leaves an
archive that no later attempt can attribute, and every retry refuses
`SyncConflict` permanently for a graph whose only authority is still the
Markdown/Org tree beside it.

A refusal from that final source proof names what moved: the row count and, for
the first rows, the exact path together with the field that changed (filesystem
resource identity, link count, or content description), and whether the row
appeared, vanished, or changed. A file that merely appears changes neither the
source-file nor the source-chunk count, so the inventory report is the only
thing that localises it.

Reported paths escape every non-ASCII scalar (`pages/\u{17d} pilot notes.md`);
ASCII paths are reported exactly as they are on disk. A graph may hold two files
whose names differ only by Unicode normalization, and those two names print as
one glyph sequence in every log and issue tracker — a refusal that named such a
row unescaped named a row nobody could tell from its neighbour. Escaping is a
reporting rule only: nothing normalizes, folds, or rewrites a name or a byte on
disk.

`Retryable` from that proof means retryable, and callers are expected to retry
rather than surface the first refusal as a failed activation. An external
editor, a filesystem sync provider, or a second window saving while Tine is
still importing is an ordinary in-scope event; the attempt retracts the
disposable archive it created, and the next attempt rebuilds from the current
Direct Files bytes. A caller that retries must still carry every refusal it
retried past, so that a graph refusing on every attempt cannot read as a graph
that never refused.

The graph-local shared-provider tree is transport rather than local authority.
Tine still creates and opens it no-follow, requires ordinary directories and
regular files, flushes published file contents, and validates bounded bytes and
digests. On Android, inability to fsync a shared-storage directory is treated
as a platform durability limit rather than a durable refusal. App-private
enrollment, archive, journal, and SQLite directory barriers remain required.
The same limit applies to the Markdown/Org projection under §2.10a; it does not
apply to graph-tree artifacts the graph is the sole authority for.

During uninterrupted activation, SQLite's terminal builder is the single
bounded producer of parser-owned terminal page states. An activation-only
consumer derives exact-source shadow manifest evidence from those same chunks,
but cannot publish it until SQLite has completed and supplied the projection
proof that names the final shadow publication. It retains compact canonical
manifest entries, never a second graph-sized page cache. A crash or later
reopen discards that process-local evidence and uses the independent sealed
source plus archive reconstruction path; differential and crash-cut tests
require the two paths to publish identical durable shadow bytes.

The ordinary release suite tests the clean baseline-plus-manifest runtime,
including activation, cold reopen, editor/application saves, external
reconciliation, cross-page moves, graph/PDF/guide reads, sharing, late join,
restart, and clean shutdown. The interleaved pre-0.7 actor scenarios remain a
differential oracle while their source is physically extracted, but they are
not allowed to redefine the production contract or make a release depend on
retired enrollment, Patricia, projection-work, scratch, or shadow mechanics.
The exact clean-runtime selection is pinned by
`scripts/tine-core-nextest-contract.mjs`; every other `tine-core` module remains
fully selected. The architectural guards that bind this document to the code —
including the §3.1 refusal-table diff
(`public_durable_refusal_scenarios_exactly_match_the_storage_contract`), the
blocked-reason vocabulary scan, and
`managed_storage_validation_is_not_unix_uid_coupled` — are release tests even
though they live in `sync_runtime::tests`. What the release gate does not run is
enumerated by name in `PRE_07_SYNC_RUNTIME_EXCLUDED_TEST_NAMES`, and the
contract fails both on an unlisted exclusion and on a listed name with no test
behind it. Adding a new production runtime journey therefore requires an
explicit contract update rather than being silently included or omitted.

Current disposable schema identities are scratch 13 / scratch page 1 / SQLite
20. Their authoritative values are `tine_storage::formats::{SCRATCH_SCHEMA_VERSION,
SCRATCH_PAGE_SCHEMA_VERSION, SQLITE_SCHEMA_VERSION}`. Bumping one invalidates
only that derived representation and costs one rebuild; it must not migrate or
reinterpret authoritative oplog bytes. Authoritative format changes require an
explicit versioned migration and cannot be treated as a cache rebuild.

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
open, save, or reload. It attaches only to Direct Files graphs; a managed
binding never attaches one (the oplog owns managed merge confidence,
invariant 8 stays intact).
