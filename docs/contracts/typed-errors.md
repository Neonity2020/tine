# Typed backend errors

Tine's Tauri command signatures still reject with strings, but any rejection
that changes frontend control flow crosses that boundary as a fixed-shape JSON
object:

```json
{"kind":"managed-actor-refusal","reason_code":"trusted_local.append_outcome_unknown"}
```

`kind` is a bounded code. `reason_code` is present only for the existing
managed-actor refusal vocabulary. Payloads never carry note text or wording
intended for display. One kind carries a typed `detail` object:
`shared-frontier-mismatch` includes the mismatch counts and at most 32
relative note paths (each `local-only`, `shared-only`, or `changed` with its
categories) plus an `omitted` count, because `docs/storage-sync-contract.md`
promises the joining user exactly that list to reconcile a refused join. The
funnel validates the detail field by field and degrades a malformed detail to
`null`; the frontend still owns every displayed word.

`TauriBackend.call` is the only frontend classification point. It converts a
recognized payload into one of the 9 BackendError subclasses (including the
existing `SaveConflictError`). Components branch with `instanceof`; the
frontend message table owns user-visible wording. Unknown and malformed
rejections keep their pre-existing generic error behavior.

Scenario I/O errors retain `std::io::ErrorKind` as
`ScenarioError::Io(ErrorKind)`, never a formatted source error. Panic flight
records contain only source location, thread name, and a content-free payload
type class (`message_kind`).

The plugin-visible boundary is separate and unchanged. Plugin workers still
reply with `{ id, ok: false, error: string }`, and the host still exposes that
as `PluginRuntimeError`; JSON-tagged application errors are not sent to guests.

## Clean-open item 3 checkpoint

The item 3 checkpoint preserves the existing `fn display` debt at exactly 89
call sites. Its source census found 16 concrete error classes across 17
functions, including private `EngineError` in dossier-Forbidden
`hot_engine.rs`, while the three named boundaries also receive strings already
flattened by a larger helper graph. An honest single `CleanOpenError` cannot
carry those sources through the public Tauri boundary without an opaque prose
variant or a second public taxonomy. The equality ratchet prevents any new
site while a later packet designs that split.
