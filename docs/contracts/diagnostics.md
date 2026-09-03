# Diagnostics content boundary

This contract enforces I-5 (graph-authored content does not enter automatic
logs, crash reports, or telemetry) and I-11 (a diagnostic helper's name and
gate agree). Diagnostics are local only; Tine does not add a telemetry channel.

## Channels

The always-on native flight recorder accepts a fixed event name plus reviewed
boolean, numeric, enum, and public build fields. It accepts no free-form
message, path, detail, page title, block text, property value, or graph-relative
path. `fixed_event_shape_contains_no_free_form_message_fields` in
`src-tauri/src/debug.rs` is the exemplar. Diagnostic reports copy this recorder,
not the directed debug log.

The src-tauri detailed helper `diag` is a no-op unless `debug_enabled()` is true
through `TINE_DEBUG=1` or `--debug`. This is an explicit, local investigation
channel and may contain a directed path or OS error; it is never folded into the
automatic recorder. A src-tauri failure that must remain always-on instead uses
a fixed-shape event or a fixed content-free terminal line.

The core crate cannot call the src-tauri event helper. Newly retained core
diagnostics are therefore content-free and use
`runtime_debug_diagnostics_enabled()`. Existing specialized performance traces
remain behind their named environment flags and are census-reviewed as directed
local investigation channels. An always-on core line is legal only when its
allowlist row explains why its error or fixed shape cannot carry user content.

Frontend `console` output is not captured, persisted, or included in reports.
Variable-bearing calls nevertheless require an exact reviewed entry. Graph
identity on the save path is represented by a count, never a page name.

## Managed runtime tick vocabulary

The native bridge emits only these bounded tick states: `idle`,
`checkpoint_capture_skipped`, `local_mutation`, `provider_mutation`,
`recovery_blocked`, `recovering`, `retry_full`, `blocked`, `failed`,
`admitted_noop`, `admitted_complete`, and `terminal`.
`checkpoint_capture_skipped` is deliberately distinct from `recovering`:
the disposable checkpoint was not captured, while accepted authority and the
foreground runtime continue unchanged. This vocabulary is pinned by
`checkpoint_capture_skip_has_its_own_tick_value`.

The fixed `managed.checkpoint_capture_skipped` receipt carries exactly one of
`runtime_not_attached`, `indexed_runtime`, `blocked_runtime`,
`unsettled_runtime`, `durable_frontier_ahead`, or `capture_failed`.
These are bounded causes, never error prose, and are pinned by
`fixed_event_shape_contains_no_free_form_message_fields`.

Parser failures cross the lsdoc-diff worker boundary only as a status plus
`ParserDiagnostic`: a nullable numeric offset, UTF-8 input length, and opaque
input hash. An exception object or free-form parser detail cannot inhabit that
type.

## Equality ratchets

`crates/tine-core/tests/content_out_of_logs.rs` walks production Rust library
sources, excluding standalone CLI output and cfg(test) regions. Its exact
allowlist currently contains 74 Rust production print sites, each with a class,
reason, and gate. A deletion changes the census just as an addition does.

`src/contentOutOfLogs.ratchet.test.ts` walks production TypeScript and TSX and
classifies 21 variable-bearing frontend console sites. It also pins the parser
failure shape and the two allowlist counts in this document. Changes to either
census require an explicit contract review.

The required repair for an unreviewed Rust site is: use a fixed-shape event
(src-tauri) or a content-free flag-gated line (core).
