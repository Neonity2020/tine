# Typed backend errors

## Query availability adapter

The frontend classifier accepts `query-not-ready` with the closed reason set
`indexing`, `recovering`, `pending_edits`, `busy` as `QueryNotReadyError`.
`query-unavailable` requires a valid dotted reason code and nonempty
`detail.message`, producing `QueryUnavailableError`. Cancellation uses the
existing `OperationCancelledError`. Unknown or malformed payloads remain
unclassified; matching prose never makes an error retryable.

`runQueryWhenReady` is the consumer-owned retry primitive: only the typed pending
error retries, using 100/200/400/800ms delays capped at 800ms. Its owner supplies an
AbortSignal, a captured monotonic request/binding revision check, and a pending
status callback. Attempts and answers check ownership; stale/aborted requests
never change a successor's status. Leaving one consumer does not cancel another
consumer's shared attempt. Native job cancellation remains a separate obligation.
`sharedQueryResult` accepts an optional subscriber signal and gives its loader
one shared attempt signal. Leaving a subscriber releases only that subscription;
the last departing subscriber aborts the shared signal unless an existing
unsignalled metadata consumer still owns the load. Abandoned late answers cannot
populate the memo or remove a newer same-key attempt. Graph scope changes govern
cache reuse; they do not revoke unrelated subscribers. Native transport must use
the shared attempt signal, never the first subscriber's signal. Inline and tag
queries pass their resource's subscriber signal to this boundary.
Wire classification, late answers, timer cleanup, source ABA, graph replacement,
terminal failures and shared-consumer isolation are tested in
`src/queryReadiness.test.ts`.

`createReadyQueryResource` binds that operation to Solid resource ownership,
source revisions, `graphBinding`, graph root and graph transition state. Inline
query results/Explain, workspace execution, and tag table/toggle reads use it.
Existing successful rows remain mounted while availability is pending. Initial
indexing is not presented as an empty result, and permanent errors are surfaced
without throwing through the result renderer. Disabling/unmounting a resource
aborts its retry owner. Component evidence lives in the corresponding Macro,
Workspace, Page and resource tests. A graph switch or binding replacement clears
old rows instead of carrying them into another graph's pending state. The binding
counter is observed through its rebind notification; display epochs only trigger
a binding check. All shared result/registry cache scopes include that binding,
so a same-root rebind cannot reuse a previous binding's cached answer.

Workspace materialization also retries readiness during validation, with the
captured save-input guard and a controller aborted by edits, graph rebinding or
teardown. No write is retried, and a write already begun keeps its existing
audited completion semantics. Export query expansion retries as one bounded
batch; typed permanent availability errors reach the modal and prevent copying
an incomplete rendered expansion. Literal fallback for other unsupported macro
resolution is unchanged. Export ownership stops on teardown or graph rebinding.

Campaign integration is still in progress: native availability producers,
per-consumer native job cancellation and production traversal retirement remain
required before RET2 acceptance.

### Published query exports (Stage 2)

A query export's `app/` runs this frontend over `app/snapshot.json` through
`src/publishedBackend.ts`, selected by `backend()` when the document carries
`<meta name="tine-published">`. Two typed refusals are specific to it:

- `query-unavailable` with reason code `published_export_static` — `parseQuery`
  or `queryRun` was asked for a query the export was not made with (a changed
  text, dialect, `tine.*` view property, or current page; a run asked under a
  view the export never ran gets the page's baked answer for that query, and a
  run asked with no page at all — a query inside a sheet cell — gets the one
  record of that query). Views are compared under `viewKey`: the engine
  writes `ViewSettings` densely (`sort: []`, `columns: []`, `aggregates: []`
  always present) and the app resolves a scoped draft sparsely, so an absent
  field and an empty list are the same key (`wire_parse.rs`
  `anchored_view_of_a_scoped_draft_serializes_densely` pins the producer
  shape, `publishedBackend.test.ts` the consumer). The refusal can come from
  either parse: the authored one, or the execution-side parse of a
  `<% current page %>` argument substituted for a page the export never ran
  it on (the macro shown in another page's Linked References); `Macro.tsx`
  reads that resource's `error` before `latest` and shows the message where
  the rows would be. Or `runGraphSearch`
  was asked on any lane but the Quick Switcher's (`quick-switch*`), which get a
  plain substring match over the snapshot's page names, aliases and block text
  for navigation. The snapshot holds answers, not an index; no query is re-run
  in a reader's browser. `detail.message` is "This export answers only the
  queries it was made with."
- `published-export-read-only` (`PublishedExportReadOnlyError`) — any `Backend`
  method classified as refused in `PUBLISHED_REFUSED_METHODS` (writes, sync,
  plugin install, capture, native pickers, OS access). The class lives in
  `backend.ts` because `publishedBackend.ts` is imported by it.

`src/publishedBackend.guard.test.ts` reads `interface Backend` through the
TypeScript AST and requires every member to be in exactly one of answered /
constant / refused / absent, so a new method cannot reach a reader's browser
unclassified. Beyond the Quick Switcher lanes, the answered set deliberately
includes two reader affordances that reach the browser, not the graph:
clipboard copy (`writeText`/`writeRich`) and `confirm`. `queryFacets` and
`referencedPageNames` answer empty: the export carries answers, not the
facet or reference inventories a sentence builder would need. Asset reads
answer only names inside the export's own `assets/` folder, under the rule
the static copier applies (`AssetSink::asset_relative` in `publish.rs`) to the
name after its `assets/` prefix: a `?` query or `#` fragment is dropped; a
remote (`://`) reference, a backslash, an absolute path, a leading `.` step or
a `..` step is refused without a request; empty and interior `.` steps
collapse, and a colon inside a file name is a name. (A percent-encoded
authored name is copied literally and requested literally by images and
media; a clicked file link decodes it first, in the app and in an export
alike — a pre-existing app-side difference, not an export rule.)
Presentation limits of the baked home page: a
sampled query shows "sample of N" beside its count, read from the anchored
section's effective view — the one the run used — (the builder sentence that
says so in the app is not offered), and an advanced (`#+BEGIN_QUERY`) home
carries no host `tine.*` properties — the home runs under the query's own
settings, as the app runs an advanced query.

## Command error boundary

Every Tauri command and helper under `src-tauri/src` now rejects with
`CommandError`; the phase-B migration removed the remaining `String` error
boundaries. `CommandError` deliberately serializes as the same JSON string
value each command emitted before the migration. A tagged
error is therefore still a string whose contents are a fixed-shape JSON object:

```json
{"kind":"direct-save-failure","reason_code":"precheck.symlink","detail":{"io_error_kind":"InvalidInput"}}
```

`kind` is a bounded code. `reason_code` is present for Direct save failures,
Direct save conflicts, query refusals and query availability answers. Payloads
never carry note text or wording intended for display. Kinds that carry a
typed `detail` object have it validated field by field; a malformed detail
degrades to `null`. `direct-save-failure` carries only the `io_error_kind`;
`save-conflict` carries that kind plus `epoch` (a non-negative integer or
`null` when no override authority exists); `query-print-refused` carries the
printer's structured `Diagnostic`. The frontend still owns every displayed
word.

`TauriBackend.call` is the only frontend classification point. It converts a
recognized payload into one of the 7 BackendError subclasses (`AssetTooLargeError`,
`QueryPrintRefusedError`, `OperationCancelledError`, `QueryNotReadyError`,
`QueryUnavailableError`, `DirectSaveFailureError`, `SaveConflictError`). Components branch with `instanceof`; the
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
| `PrecheckNotPortable` | `precheck.not_portable` | no retry | portable-path admission |
| `PrecheckNofollow` | `precheck.nofollow` | no retry | retained-directory admission |
| `PrecheckLimit` | `precheck.limit` | no retry | bounded inventory |
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
| `RefusedDataPreservation` | `refused.data_preservation` | no retry | data-preservation firewall |
| `Unknown` | `unknown` | retry | unclassified source failure |

The frontend save-policy vocabulary (`isRetryableSaveFailure` in
`src/persistence.ts`) is pinned to a subset of these 36 strings; the ratchet in
`src/typedErrorRatchet.test.ts` fails on a frontend code no Direct producer
emits.

The plugin-visible boundary is separate and unchanged. Plugin workers still
reply with `{ id, ok: false, error: string }`, and the host still exposes that
as `PluginRuntimeError`; JSON-tagged application errors are not sent to guests.

## `CommandError` boundary

The variants are `Tagged`, `Coded`, `Io`, `Worker`, `Tauri`, `Json`, `Plugin`,
`Clipboard`, `Platform`, `GraphVerification`, `Graph`, `StorageTransition`,
`Settings`, `Diagnostic`, `Backup`, `Core`, and `Prose`. Custom `Serialize`
always calls `serialize_str`; an object-valued IPC
wire is a future product decision, not part of wave 4. `From<String>` and
`From<&str>` are intentionally absent. `Worker` is constructed only at
`spawn_blocking(...).await` joins; other `tauri::Error` values use `Tauri`.
`DirectSaveError` never reaches the generic `Io` conversion because doing so
would discard its closed reason code and conflict epoch.

### Conversion table

| Source type | Production mapper | Variant | Format family | Owned producer symbols |
| --- | --- | --- | --- | --- |
| `DirectSaveError` | `direct_save_error_message` | `Tagged` | tagged reason + detail | `direct_save_error_message` conflict and failure branches |
| clean shutdown refusal | `CommandError::tagged` | `Tagged` | tagged kind | `close_graph_window` |
| `std::io::Error` | `CommandError::from` | `Io` | source display | filesystem/`Graph` calls in `commands.rs` and `state.rs` |
| `tauri::Error` from blocking join | `CommandError::worker` | `Worker` | source display | every `spawn_blocking(...).await` site in `commands.rs` |
| other `tauri::Error` | `CommandError::from` | `Tauri` | source display | path/window/platform calls in `commands.rs` |
| bounded code + detail | `CommandError::coded` | `Coded` | `{code}: {detail}` | query/result budgets, base64 decoding, retained-page conversion, canonical path context |
| `serde_json::Error` | `CommandError::from` / `CommandError::json` when legacy context is retained | `Json` | source display or unchanged contextual display | JSON producers in `conflict_capsule.rs`, `graph_verification.rs`, `plugins.rs`, and `settings.rs` |
| plugin package/registry source | `CommandError::plugin` | `Plugin` | unchanged source/context display | plugin validation, recovery, publication, and retirement helpers |
| clipboard provider source | `CommandError::clipboard` | `Clipboard` | unchanged source display | `read_clipboard_files`, `copy_image_to_clipboard` |
| platform/host source | `CommandError::platform` | `Platform` | unchanged source/context display | platform opener and capture-host helpers |
| graph-verification source | `CommandError::graph_verification` | `GraphVerification` | unchanged source/context display | verification task, dialog, and report helpers (non-JSON) |
| graph lifecycle source | `CommandError::graph` | `Graph` | unchanged source/context display | `graph.rs` and `watcher.rs` lifecycle helpers |
| storage transition source | `CommandError::storage_transition` | `StorageTransition` | unchanged source/context display | `storage_transition_supervisor.rs` helpers |
| settings source | `CommandError::settings` | `Settings` | unchanged source/context display | settings validation/load helpers |
| diagnostic source | `CommandError::diagnostic` | `Diagnostic` | unchanged source/context display | diagnostic recorder and dialog helpers |
| backup source | `CommandError::backup` | `Backup` | unchanged source/context display | backup validation and restore helpers |
| literal/context-only remainder | `CommandError::prose` | `Prose` | unchanged prose | symbols in the census below |

The source manifest in `command_error::tests` records `targets`, `file`,
`enclosingSymbol`, `sourceErrorType`, `requiredVariant`, `productionMapper`,
`formatFamily`, `legacyWireTemplate`, and `goldenTest`. Every format family has
a producer-coupled golden in
`command_error::tests::phase_a_production_wire_matches_legacy` and
`command_error::tests::phase_b_production_wire_matches_legacy`; the structural
guard expands every `CommandError` mapper occurrence into a
file/enclosing-symbol/mapper site row, equality-pins the 248-row manifest and
its placement fingerprint, rejects stale registration/manifest rows, and
rejects a typed source routed to `Prose`. The family rows supply source type,
required variant, format family, legacy wire template, and producer-coupled
golden for those site rows.

### Absolute phase-B rule

Every fallible command registered for desktop, Android, or iOS returns
`Result<_, CommandError>`. Every helper under `src-tauri/src` is subject to the
same error boundary: `Result<_, String>` and the temporary
`map_err(|error| error.to_string())` bridge are both strict zero, with no
third-party exception rows. `Worker` is used only immediately after
`spawn_blocking(...).await`; non-worker Tauri errors remain `Tauri`.

### Conversions inside a platform `cfg` body

`CommandError` implements `From` for `std::io::Error`, `tauri::Error` and
`serde_json::Error`, and for nothing else.
Two native error types you will meet only inside a platform `cfg` body are
therefore NOT convertible with `CommandError::from`:

| source | where | mapper |
| --- | --- | --- |
| `PluginInvokeError` (`run_mobile_plugin`) | Android, iOS | `CommandError::platform` |
| `tauri_plugin_opener::Error` (`open_url`) | Windows, Android, iOS | `CommandError::platform` |

`platform` is the family for an OS capability — a folder picker, media capture,
system bars, opening a URL. `plugin` means the **Tine** plugin system; its only
producer is `plugins.rs`. Prefer the family constructor over adding a `From`
impl: `From` widens what inference will silently accept, which is the surface
this contract exists to keep narrow.

**Nothing on a Linux host compiles any of these bodies.** W4-E2b wrote
`CommandError::from` at nine such sites, passed every local gate, and hosted CI
then failed Android with nine errors and Windows with one. The two iOS sites
were not caught by CI at all — no job compiles iOS — only by enumerating all
five shipped targets by hand, the same gap that left `rename_noreplace` without
an iOS arm through v0.10.0. When you touch a `cfg`-gated conversion, enumerate
Linux, Windows, macOS, iOS and Android deliberately, and expect CI to be the
first thing that compiles what you wrote.

Guard: `backend_command_parity::tests::native_platform_calls_convert_through_a_family_constructor`.
Exemplar to imitate: `android_media::call`.

### `Prose` census

The syntactic census is 46 production sites (34 in `commands.rs`, 12 in
`state.rs`; test modules excluded). `CommandError::prose` is an identity
adapter when a phase-B helper already returns `CommandError`, so those retained
E2 call sites do not erase the typed variant. The rows below have no typed
source: they are literal state assertions, bounded domain wording, or
wrong-reply outcomes.

History: 113 → 116 (2026-09-05, P0-rust Wave B) added three managed
wrong-reply arms; 116 → 119 (2026-09-07, P2) added the device-local notice
store literals; 119 → 47 (2026-09-15, Managed Storage removal) retired the
managed command surface, its wrong-reply arms and the quit-preparation
fixtures, net of the one query-export refusal pass-through merged the same
day. No legacy prose site was reintroduced or converted back from a typed
variant. A future growth entry belongs in this paragraph with the same three
facts: which commands, which arms, and what did *not* regress.

116 → 123 (2026-09-14, publish-query stage 1): `publish_query_plan` and
`publish_query` each carry the three wrong-reply/deferred arms every
`sparse_application_handle` command carries (`Published`/`Planned` wrong
reply, `Refused { message }`, `Deferred`), and `query_publication_error` passes
the core's `QueryPublicationError::Refused(String)` — a user-facing refusal
composed in `tine-core` with no source error — through as prose. The budget
refusal is NOT prose: it is a tagged `query-unavailable` /
`export_asset_budget_exceeded` error. No legacy prose site was retained,
reintroduced, or converted back from a typed variant; the retirement owners
below are unchanged.

| File | Enclosing symbols | Legacy template | Why no typed source exists | Retirement owner |
| --- | --- | --- | --- | --- |
| `commands.rs` | `publish_query_plan`, `publish_query`, `query_publication_error` | wrong-reply/deferred/refusal wording | enum outcome has no error value in that arm; core refusal is composed prose | outcome taxonomy follow-up |
| `state.rs` | `bind` | authority/lease/binding literal or bounded contextual message | local state predicate, not a source error | typed state domain follow-up |
| `state.rs` | `owned_graph_context`, `canonical_graph_root`, `slot_for_window`, `slot_for_bound_window`, `capture_quick_switch_slot`, `refresh_graph_for_label` | missing/stale/bound-window/canonical-path literal | local state predicate or E2b bridge | W4-E2b |
| `commands.rs` | `load_workspaces`, `save_workspaces`, `open_page_file` | unchanged helper display | E2 compatibility adapter; typed phase-B errors pass through unchanged | phase-A adapter cleanup |
| `commands.rs` | `journal_feed_page`, `get_backlink_filter_context`, `print_error`, `export_query_subtrees`, `run_advanced_query`, `resolve_blocks` | bounded-size or wrong-reply wording | bounded/local semantic refusal | query outcome taxonomy follow-up |
| `commands.rs` | `decode_asset_b64`, `read_local_image`, `import_native_capture`, `delimited_ext`, `open_asset`, `edit_asset_external`, `build_editor_argv` | local validation/platform literal | validation branch has no source error | validation taxonomy follow-up |
| `commands.rs` | `conflict_capsule_diff`, `merge_pages` | authority missing/changed literal | local state predicate | conflict taxonomy follow-up |
| `backup.rs` | `collect`, `collect_scoped_restore_graph_text`, `restore_backup`, `restore_from_backup_source` | backup path/schema/safety literal | local validation branch, not a source error | backup outcome taxonomy follow-up |
| `conflict_capsule.rs` | `capsule_page_name`, `decode_envelope`, `quarantine_unreadable`, `reclaim_torn_temps`, `retire_at`, `write_unlocked` | capsule validation literal | local validation branch, not a source error | capsule outcome taxonomy follow-up |
| `debug.rs` | `clear_diagnostics`, `save_diagnostic_report` | recorder/destination literal | local availability or validation branch | diagnostic outcome taxonomy follow-up |
| `graph.rs` | `approve_external_assets`, `capture_graph_binding`, `capture_target_for_state`, `create_graph`, `inspect_graph_access`, `load_graph`, `load_graph_for_label`, `open_graph_window` | graph state/selection literal | local predicate, not a source error | graph outcome taxonomy follow-up |
| `graph_verification.rs` | `cancel_graph_verification`, `create_graph_verification`, `save_graph_verification_report` | operation/registry/destination literal | local predicate, not a source error | verification outcome taxonomy follow-up |
| `lib.rs`, `android_folder_picker.rs` | `capture_frontend_ready`, `pick_graph_folder` | platform availability literal | local platform predicate | platform outcome taxonomy follow-up |
| `android_media.rs` | `$name` in the non-Android `android_media_command` template | unsupported-platform literal | cfg-split macro command has no source error | platform outcome taxonomy follow-up |
| `platform.rs` | `external_open_plan`, `open_external`, `reveal_page_source` | unsupported URL/platform/path literal | local validation branch | platform outcome taxonomy follow-up |
| `plugins.rs` | `install_plugin`, `install_plugin_package_at`, `manifest_identity`, `package_dir`, `plugins_dir`, `read_plugin_entry`, `set_plugin_enabled`, `set_plugin_enabled_at`, `store_plugin_registry_cache`, `store_plugin_registry_cache_at`, `uninstall_package`, `validate_uninstall_target`, `verify_plugin_registry` | plugin identity/bounds/availability literal | local validation branch, not a source error | plugin outcome taxonomy follow-up |
| `settings.rs` | `atomic_write_workspaces`, `load_notices`, `load_session`, `load_workspaces`, `migrate_legacy_session_at`, `reveal_known_graph`, `save_notices`, `save_notices_at`, `save_session`, `save_session_at`, `save_workspaces`, `update_settings`, `validate_workspaces_json` | settings shape/availability literal | local validation branch, not a source error | settings outcome taxonomy follow-up |
| `storage_transition_supervisor.rs` | `commit_if_current` | superseded-transition literal | local state predicate | transition outcome taxonomy follow-up |

