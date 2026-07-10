# 0039. The canonical graph root is a hard filesystem boundary

- **Status:** Accepted
- **Date:** 2026-07-10

## Context

Logseq configuration controls the page and journal directories and the journal
filename pattern. Treating those strings as trusted paths lets an absolute path,
`..`, or an escaping symlink route ordinary saves, deletes, snapshots, and restores
outside the graph selected by the user. Multi-window ownership also cannot be safe
when one open graph is nested inside another graph's recursively scanned tree.

## Decision

- Runtime graph opening uses a canonical root and validates configured managed
  directories as contained relative paths.
- Every path-addressed save validates its concrete target against that root; a
  formatted journal stem is data, not an implicitly trusted path.
- The window registry rejects equal, ancestor, and descendant graph roots owned by
  different windows.
- Backup identity derives from a cryptographic digest of the canonical root and
  every restorable snapshot records that root in a verified manifest.
- Invalid layouts fail closed with a clear error. Tine never substitutes another
  directory and writes there silently.

## Consequences

Safe nested relative directories remain supported. Graphs that intentionally point
their managed directories outside the selected root must move those files inside
the graph before Tine will write them.
