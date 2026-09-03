# Typed backend errors

Tine's Tauri command signatures still reject with strings, but any rejection
that changes frontend control flow crosses that boundary as a fixed-shape JSON
object:

```json
{"kind":"managed-actor-refusal","reason_code":"trusted_local.append_outcome_unknown"}
```

`kind` is a bounded code. `reason_code` is present for managed-actor refusals,
Direct save failures, and Direct save conflicts. Payloads never carry note text
or wording intended for display. Three kinds carry typed `detail` objects.
`shared-frontier-mismatch` includes the mismatch counts and at most 32
relative note paths (each `local-only`, `shared-only`, or `changed` with its
categories) plus an `omitted` count. `direct-save-failure` carries only the
`io_error_kind`; `save-conflict` carries that kind plus `epoch` (a non-negative
integer or `null` when no override authority exists). This is because
`docs/storage-sync-contract.md`
promises the joining user exactly that list to reconcile a refused join. The
funnel validates the detail field by field and degrades a malformed detail to
`null`; the frontend still owns every displayed word.

`TauriBackend.call` is the only frontend classification point. It converts a
recognized payload into one of the 10 BackendError subclasses (including the
existing `SaveConflictError`). Components branch with `instanceof`; the
frontend message table owns user-visible wording. Unknown and malformed
rejections keep their pre-existing generic error behavior.

## Direct save failures

The Direct producer retains `DirectSaveError` inside the public `io::Error`
surface. `DirectSaveFailureCode` and the optional conflict epoch are typed
fields; the source error is display-only. `direct_save_failure_code` and
`direct_save_conflict_epoch` downcast that inner value and never inspect
`io::Error::to_string()`.

The old whole-string `conflict` / `conflict:<epoch>` wire is retired. Ordinary
failures now use:

```json
{"kind":"direct-save-failure","reason_code":"precheck.symlink","detail":{"io_error_kind":"InvalidInput"}}
```

Banner-class conflicts use the existing tagged kind:

```json
{"kind":"save-conflict","reason_code":"conflict.pinned_owner","detail":{"io_error_kind":"AlreadyExists","epoch":17}}
```

| Variant | Stable string | Disposition | Producing stage |
| --- | --- | --- | --- |
| `PrecheckSymlink` | `precheck.symlink` | no retry | no-follow inventory |
| `PrecheckInterrupted` | `precheck.interrupted` | retry | coherent capture |
| `PrecheckPortableCollision` | `precheck.portable_collision` | no retry | portable-name admission |
| `PrecheckResourceAlias` | `precheck.resource_alias` | no retry | physical-resource admission |
| `PrecheckNotPortable` | `precheck.not_portable` | no retry | managed-path admission |
| `PrecheckNofollow` | `precheck.nofollow` | no retry | retained-directory admission |
| `PrecheckLimit` | `precheck.limit` | no retry | bounded inventory |
| `IdentityChangedSinceLoad` | `identity.changed_since_load` | retry | retained loaded identity |
| `IdentityOwnedElsewhere` | `identity.owned_elsewhere` | no retry | semantic owner check |
| `IdentityNameTaken` | `identity.name_taken` | no retry | rename/create identity |
| `ConflictRetrySaveBaselinePresent` | `conflict_retry.save_baseline_present` | retry | tokenless present baseline |
| `ConflictRetrySaveBaselineAbsent` | `conflict_retry.save_baseline_absent` | retry | tokenless absent baseline |
| `ConflictRetryCommitRecheck` | `conflict_retry.commit_recheck` | retry | tokenless commit recheck |
| `ConflictRetryReplacePreRetirement` | `conflict_retry.replace_pre_retirement` | retry | tokenless replace pre-retire |
| `ConflictRetryReplaceRetiredMismatch` | `conflict_retry.replace_retired_mismatch` | retry | tokenless retired recheck |
| `ConflictRetryReplacePublicationCollision` | `conflict_retry.replace_publication_collision` | retry | tokenless replace publish |
| `ConflictRetryCreatePublicationCollision` | `conflict_retry.create_publication_collision` | retry | tokenless create publish |
| `ConflictRetryFinalRereadAbsent` | `conflict_retry.final_reread_absent` | retry | tokenless final absent read |
| `ConflictRetryFinalRereadPresent` | `conflict_retry.final_reread_present` | retry | tokenless final present read |
| `ConflictRetryReplacePostPublication` | `conflict_retry.replace_post_publication` | retry | tokenless post-publish validation |
| `ConflictAuthoritySuperseded` | `conflict_authority.superseded` | re-observe | override epoch check |
| `ConflictAuthorityOtherEpisode` | `conflict_authority.other_episode` | re-observe | editor-episode check |
| `ConflictAuthoritySpent` | `conflict_authority.spent` | re-observe | one-shot authority check |
| `ConflictSaveBaselinePresent` | `conflict.save_baseline_present` | banner | present baseline observation |
| `ConflictSaveBaselineAbsent` | `conflict.save_baseline_absent` | banner | absent baseline observation |
| `ConflictCommitRecheck` | `conflict.commit_recheck` | banner | commit recheck |
| `ConflictReplacePreRetirement` | `conflict.replace_pre_retirement` | banner | replace pre-retire |
| `ConflictReplaceRetiredMismatch` | `conflict.replace_retired_mismatch` | banner | retired recheck |
| `ConflictReplacePublicationCollision` | `conflict.replace_publication_collision` | banner | replace publication |
| `ConflictCreatePublicationCollision` | `conflict.create_publication_collision` | banner | create publication |
| `ConflictFinalRereadAbsent` | `conflict.final_reread_absent` | banner | final absent read |
| `ConflictFinalRereadPresent` | `conflict.final_reread_present` | banner | final present read |
| `ConflictReplacePostPublication` | `conflict.replace_post_publication` | banner | post-publish validation |
| `ConflictPinnedOwner` | `conflict.pinned_owner` | banner | exact pinned owner |
| `ConflictBaseRev` | `conflict.base_rev` | banner | base revision |
| `Unknown` | `unknown` | retry | unclassified source failure |

The frontend save-policy vocabulary is pinned to the union of these 36 strings
and the 22 `SyncEditorRefusalCode` strings. The pre-existing
`managed.conflict` tagged prefix is the single documented exception: it remains
owned by the Managed producer and is cut to E2/E2b rather than being folded into
the Direct enum.

The managed half of that generated union is:

```text
trusted_local.missing_base_revision
trusted_local.preparation.bindings
trusted_local.preparation.planning
trusted_local.preparation.draft
trusted_local.preparation.capture
trusted_local.preparation.finalize
trusted_local.preparation.publication
trusted_local.preparation.archive_stage
trusted_local.preparation.sqlite_drain
trusted_local.preparation.projection_drain
trusted_local.engine_authority
trusted_local.commit.invalid_prepared_input
trusted_local.commit.managed_record
trusted_local.commit.precommit_graph
trusted_local.commit.append_refused
trusted_local.append_outcome_unknown
fallback.readmission
post_commit.current_page_lookup
trusted_outcome.declined
managed_queue.sequence_overflow
managed_queue.monotonicity
managed_record.decode
```

Scenario I/O errors retain `std::io::ErrorKind` as
`ScenarioError::Io(ErrorKind)`, never a formatted source error. Panic flight
records contain only source location, thread name, and a content-free payload
type class (`message_kind`).

The plugin-visible boundary is separate and unchanged. Plugin workers still
reply with `{ id, ok: false, error: string }`, and the host still exposes that
as `PluginRuntimeError`; JSON-tagged application errors are not sent to guests.

## Core-only clean-open boundary

`CleanOpenError` is the one core taxonomy for the 16 error classes reachable
from clean managed construction and recovery. It is `pub(crate)`: several
source types are crate-private, and wave 4 deliberately keeps
`SyncRuntimeOpenStatus::OpenRefused { detail: String }` as the Tauri-facing
boundary. The sole projection into that string is tagged JSON with
`kind: "clean-open"` and the stable `reason_code` below. Source display text,
paths, and note names do not enter the payload. The frontend DTO bridge and its
current parsing of open-status detail are follow-up work; this packet adds no
frontend subclass.

| Variant | Source class | `reason_code` | Refusal scenario |
| --- | --- | --- | --- |
| `BootstrapStreamingImport` | `BootstrapStreamingImportError` | `clean_open.bootstrap_streaming_import` | malformed or over-bound inbound source (`MS-REF-MALFORMED-IMPORT`, `MS-REF-BOUNDS`) |
| `Engine` | `EngineError` | `clean_open.engine` | invalid retained causal/archive state (`MS-REF-DISK-CORRUPT`) |
| `Enrollment` | `EnrollmentError` | `clean_open.enrollment` | damaged, incompatible, contended, or unsafe enrollment authority (`MS-REF-DISK-CORRUPT`, `MS-REF-PROTOCOL-INCOMPATIBLE`, `MS-REF-CONCURRENT-WRITER`, `MS-REF-UNSAFE-FS-KIND`) |
| `ManagedLocalRecord` | `ManagedLocalRecordError` | `clean_open.managed_local_record` | damaged foreground journal record (`MS-REF-DISK-CORRUPT`) |
| `ProjectionStore` | `ProjectionStoreError` | `clean_open.projection_store` | damaged, incompatible, or unsafe receipt state (`MS-REF-DISK-CORRUPT`, `MS-REF-PROTOCOL-INCOMPATIBLE`, `MS-REF-UNSAFE-FS-KIND`) |
| `ProjectionTurnJournal` | `ProjectionTurnJournalError` | `clean_open.projection_turn_journal` | damaged, contended, or unsafe turn journal (`MS-REF-DISK-CORRUPT`, `MS-REF-CONCURRENT-WRITER`, `MS-REF-UNSAFE-FS-KIND`) |
| `Receipt` | `ReceiptError` | `clean_open.receipt` | malformed or incompatible retained receipt (`MS-REF-DISK-CORRUPT`, `MS-REF-PROTOCOL-INCOMPATIBLE`) |
| `RuntimePromotion` | `RuntimePromotionError` | `clean_open.runtime_promotion` | stale or unavailable runtime authority (`MS-REF-STALE-GENERATION`, `MS-REF-CONCURRENT-WRITER`) |
| `Scenario` | `ScenarioError` | `clean_open.scenario` | malformed, conflicting, or unsafe provider input (`MS-REF-MALFORMED-IMPORT`, `MS-REF-SYNC-CONFLICT`, `MS-REF-UNSAFE-FS-KIND`) |
| `Store` | `StoreError` | `clean_open.store` | damaged, incompatible, or unsafe operation archive (`MS-REF-DISK-CORRUPT`, `MS-REF-PROTOCOL-INCOMPATIBLE`, `MS-REF-UNSAFE-FS-KIND`) |
| `Sweep` | `SweepError` | `clean_open.sweep` | damaged retained disposition state (`MS-REF-DISK-CORRUPT`) |
| `WorkspaceAuthority` | `WorkspaceAuthorityRefusal` | `clean_open.workspace_authority` | another instance or a stale lease identity owns the workspace (`MS-REF-CONCURRENT-WRITER`, `MS-REF-STALE-GENERATION`) |
| `SqliteProjection` | `oplog::sqlite::ProjectionError` | `clean_open.sqlite_projection` | contended, stale, or damaged disposable projection open (`MS-REF-CONCURRENT-WRITER`, `MS-REF-STALE-GENERATION`, with rebuild for cache-only damage) |
| `Projection` | `oplog::projection::ProjectionError` | `clean_open.projection` | malformed source or stale projection authority (`MS-REF-MALFORMED-IMPORT`, `MS-REF-STALE-GENERATION`) |
| `Io` | `std::io::Error` (retained as `ErrorKind`) | `clean_open.io` | transient storage unavailability is retryable; unsafe or damaged authority uses the corresponding §3.1 scenario |
| `Batch` | `tine_storage::BatchError<CoreDurableBatchContract>` | `clean_open.batch` | malformed or damaged durable batch (`MS-REF-MALFORMED-IMPORT`, `MS-REF-DISK-CORRUPT`) |

Reachability is a source-class × boundary property, not a Cartesian promise.
`yes` means that boundary calls a producer of the class; `—` means it does not.
The three shared-inspection entry points are grouped because they all call the
same `ScenarioError` provider primitives.

| Source class | shared provider/descriptor inspection | local activation | existing managed reopen | retained actor adoption/reopen |
| --- | --- | --- | --- | --- |
| `BootstrapStreamingImportError` | — | yes | — | — |
| `EngineError` | — | yes | yes | yes |
| `EnrollmentError` | — | yes | yes | yes |
| `ManagedLocalRecordError` | — | yes | yes | — |
| `ProjectionStoreError` | — | yes | yes | — |
| `ProjectionTurnJournalError` | — | yes | yes | yes |
| `ReceiptError` | — | yes | yes | — |
| `RuntimePromotionError` | — | yes | yes | yes |
| `ScenarioError` | yes | yes | yes | yes |
| `StoreError` | — | yes | yes | yes |
| `SweepError` | — | yes | yes | yes |
| `WorkspaceAuthorityRefusal` | — | yes | yes | — |
| `oplog::sqlite::ProjectionError` | — | yes | yes | yes |
| `oplog::projection::ProjectionError` | — | yes | yes | — |
| `std::io::Error` | — | yes | yes | yes |
| `tine_storage::BatchError<CoreDurableBatchContract>` | — | yes | yes | — |
