# 0058. Diagnostics are a privacy-safe flight recorder, not a debug build

- **Status:** Accepted
- **Date:** 2026-08-25

## Context

Many failures happen only on a reporter's Windows or mobile device. The first
report therefore often identifies which measurement or state transition was
missing, and Tine gains one more directed log line only after an extra
reporter/developer round trip. A separate “debug build” does not solve this:
the unexpected failure has already happened, it changes the binary being
tested, and a general trace can expose paths, page names or note content.

## Decision

Every production build keeps a bounded flight recorder covering the current
and previous run. Its API accepts only fixed event kinds, catalogued IPC command
names, enums, booleans, counts and durations. It cannot accept arbitrary
messages, paths, page titles, queries, URLs, note content or credentials. The
recorder is stored in private app data, rotated at one MiB per segment, and is
never uploaded automatically.

Settings → Diagnostics creates a human-previewable JSON report only when the
user asks. The user then chooses whether to copy or save it. The existing
`TINE_DEBUG=1` / `--debug` trace remains a separate, opt-in directed tool; it
may contain environment values and errors and is never included in the safe
report. Exact-commit native symbols and hidden frontend source maps are retained
as private release-CI artifacts, not embedded in public packages.

## Consequences

Ordinary bug reports can contain the build, platform, previous unclean exit,
slow/failed IPC, frontend stalls, storage transition phases, watcher latency and
Direct Files save counters without a special reproduction build. The field
allowlist and separation from detailed logging are security boundaries and must
remain contract-tested. Native minidumps and OS hang dumps are useful future
layers, but require separate consent, redaction and platform-specific handling;
they are not silently added to this report.
