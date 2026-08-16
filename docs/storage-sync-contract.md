# Managed storage and sync contract

This document is the implementation contract for Tine's opt-in managed-storage
runtime. Direct Files is the default product path and selects a mutually
exclusive `Legacy(Graph)` runtime before graph open. When Direct Files is
selected, no code below may inspect or modify `.tine-sync`, open an oplog,
create managed scratch state, or start managed recovery.

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
tick naming that batch. The production watcher treats that tick as an
observable graph change and schedules a continuation for any remaining
provider work. A terminal quiet watcher admission remains `AdmittedNoop` and
must not manufacture a frontend refresh or conflict.

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
path identity. External reconciliation reads the affected path owners from the
frontier-matched SQLite projection, reproves the corresponding baseline or
latest-manifest bytes against the engine, and then uses the same structural
page/block matcher as the established importer. It must not ask a native
Patricia path index to duplicate SQLite ownership.

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

The graph-local shared-provider tree is transport rather than local authority.
Tine still creates and opens it no-follow, requires ordinary directories and
regular files, flushes published file contents, and validates bounded bytes and
digests. On Android, inability to fsync a shared-storage directory is treated
as a platform durability limit rather than a durable refusal. App-private
enrollment, archive, journal, and SQLite directory barriers remain required.

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
fully selected. Adding a new production runtime journey therefore requires an
explicit contract update rather than being silently included or omitted.

Current disposable schema identities are scratch 13 / scratch page 1 / SQLite
20. Their authoritative values are `tine_storage::formats::{SCRATCH_SCHEMA_VERSION,
SCRATCH_PAGE_SCHEMA_VERSION, SQLITE_SCHEMA_VERSION}`. Bumping one invalidates
only that derived representation and costs one rebuild; it must not migrate or
reinterpret authoritative oplog bytes. Authoritative format changes require an
explicit versioned migration and cannot be treated as a cache rebuild.
