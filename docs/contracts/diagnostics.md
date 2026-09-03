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
