# Diagnostic report contract

Tine has two deliberately separate diagnostic channels.

1. The production flight recorder is always on, bounded, and privacy-safe by
   construction. Settings → Diagnostics previews and exports it on demand.
2. `TINE_DEBUG=1` / `--debug` is an opt-in detailed trace for a directed
   investigation. It is never copied into the production report.

## Privacy boundary

Flight-recorder writers may submit only fixed event kinds, known Tauri command
names, fixed enums, booleans, counters and elapsed times. They must never accept
or derive note content, filesystem paths, graph labels, window labels, page
titles, queries, URLs, credentials, error strings, arbitrary messages or the
detailed debug log. New event fields require a privacy review and a regression
test at the writer boundary, not merely removal in the report renderer.

Nothing is sent over the network. A report exists outside private app data only
after the user presses Copy or Save and chooses what to do with it.

## Retention and lifecycle

The native recorder keeps `current` and `previous` sessions with one rotated
older segment for each. Each segment is at most 1 MiB. A private session marker
distinguishes an ordinary shutdown from a killed/crashed run. A per-process lock
prevents a forwarded single-instance launch from rotating the primary process's
evidence. “Clear recorded events” removes all retained segments and begins a new
empty current segment.

## Report schema v1

The JSON report contains validated build metadata, OS/architecture, privacy
claims, aggregate graph-binding modes, content-free active storage transitions,
content-free watcher latency receipts, and the safe current/previous event
segments. Unknown/corrupt JSONL records are skipped. The report never reads the
opt-in detailed log.

## Release evidence

Release builds retain line-level native debug information and hidden frontend
source maps in an exact-SHA GitHub Actions artifact named
`diagnostic-symbols-<lane>-<commit>`. Source maps are removed from `dist` before
Tauri embeds it. Symbol artifacts are private build evidence, not release assets.
