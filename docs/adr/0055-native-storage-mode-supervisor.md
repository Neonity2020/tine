# 0055. One native supervisor owns storage-mode transitions

- **Status:** Accepted — implementation in progress
- **Date:** 2026-08-16
- **Amends:** [0049](0049-oplog-first-sparse-storage.md)

## Context

Graph lookup, Direct Files opening, managed recovery, storage activation, return
to Direct Files, native progress, and frontend startup recovery evolved as
separate state machines. Their local rules did not compose: a return request
could queue behind the managed open it was meant to escape, stale open progress
could be displayed as return progress, open completion could revoke a queued
return token, and a frontend inactivity timer could invent a terminal native
failure. Deleting graph-local transport data could not escape because the
managed selector and runtime live in private app data.

The recurring problem is supervisory, not evidence that accepted oplog history,
SQLite projection, or provider protocols need another rewrite. Direct Files and
managed storage must both remain first-class while managed storage is
experimental.

## Decision

One native `StorageModeSupervisor` owns native operation IDs, priority, typed
phases, cancellation/supersession, stable-mode publication, and exactly one
terminal outcome. Operations name their exact window, canonical graph root,
and kind. There is no app-global long-lived transition lock: work on different
canonical roots has independent lanes, while final registry publication is a
short compare-and-publish against the current operation ID. A stuck recovery
for graph A cannot delay opening graph B or later overwrite it, even when both
requests came from the same window. Late work may mutate state only while its
operation ID is current. The frontend subscribes before starting work,
renders only the current operation ID, and sends actions back to the supervisor;
it does not infer authority from phase strings or inactivity.

Return to Direct Files has two explicit operations. Graceful return drains a
healthy managed actor and proves its committed projection before selecting
Direct Files. Emergency return is always available from managed startup and
refusal screens. It atomically quarantines the private managed selector, blocks
older managed publication, and opens the current Markdown/Org tree directly.
It does not first open, repair, drain, archive, or recover managed storage.
Managed evidence is preserved with a warning that it may be newer than the
Markdown projection. Re-enabling managed storage starts afresh from the live
Markdown tree and never silently resurrects quarantined authority.

The migration is Direct-first. Ordinary Direct open, edit, save, and restart
must remain green at every integration boundary. The pure transition model and
typed contract land first; startup/Direct, emergency return, managed
open/activation/join, and graceful return then move behind it in that order.
Old frontend attempt authority, prefix-routed progress, watchdog terminal
outcomes, startup recovery token composition, and command-local transition lock
ownership are deleted after their consumers move.

## Consequences

Storage transitions become independently model-testable, emergency escape no
longer depends on the failing subsystem, and stale workers cannot publish after
a newer user decision. The frontend becomes a renderer rather than a second
recovery authority. Native code must carry typed operation context through
long-running boundaries and provide explicit cancellation checkpoints. During
the bounded migration, the supervisor's legacy lock bridge remains visible and
must not become a permanent second API.

Completion requires deterministic transition/crash-cut tests and exact real-app
journeys for Direct restart, managed restart, forced-kill recovery, emergency
return during recovery, missing/corrupt managed state, graceful return, and
two-device convergence, followed by physical Android acceptance.
