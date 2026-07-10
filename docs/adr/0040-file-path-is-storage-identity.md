# 0040. File path, not logical page name, is storage identity

- **Status:** Accepted
- **Date:** 2026-07-10

## Context

Two physical files can resolve to one Logseq page name: duplicate-day journals,
sync-conflict leftovers, and nested/flat basename collisions. The Rust backend can
load and save an exact graph-relative path, but the frontend working set historically
collapsed pages by logical name and could therefore display or save the wrong file.

## Decision

- A routed exact-path load must replace or coexist distinctly from any same-name
  canonical file; it may never be discarded merely because the name is loaded.
- Persistence always echoes the exact path from which a file was loaded.
- The long-term store identity is an opaque path-derived `PageId`; logical name is
  a lookup/display property and new unsaved pages use a temporary logical identity
  until their first save returns a concrete path.
- Dirty state, baselines, conflicts, tombstones, undo, and watcher reload decisions
  must follow storage identity whenever simultaneous duplicates are represented.

## Consequences

The immediate wrong-target guard replaces a same-name slot with the explicitly
requested file. Full simultaneous duplicate representation remains an incremental
store refactor, but no exact-path navigation may edit the canonical file by mistake.
