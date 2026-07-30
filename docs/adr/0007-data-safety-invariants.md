# 0007. Data-safety invariants: never silently overwrite

- **Status:** Accepted
- **Date:** 2026-06-29

## Context

Tine edits the user's real notes, *concurrently with* Logseq desktop, Logseq mobile,
and Syncthing writing the same files. Two representations of a page exist while
editing (frontend store + backend cache, see
[0002](0002-solidjs-frontend-owns-the-edit-tree.md)), and the OS save path is
check-then-rename, not an atomic compare-and-swap. The default failure mode of a
naive notes app here is **silent data loss** — overwriting an edit that landed from
another device. That is the one outcome a notes tool must not have.

## Decision

We will treat data safety as a first-class invariant set, not best-effort:

- **Never silently overwrite a file that changed on disk** — detect it (per-file
  rev/baseline) and surface a **conflict** instead.
- **Atomic writes** (`atomic_write` + fsync); **skip byte-identical rewrites** so we
  don't create Syncthing diff churn; preserve each file's exact formatting.
- **Recoverable deletion** — page/asset delete moves to a **trash**, never `unlink`;
  deleting a block never deletes its referenced media.
- **Launch snapshots** with one-click restore (and a safety snapshot taken before a
  restore).
- **`.org` is rewritten only when reproducible byte-for-byte**; anything we can't
  round-trip loads **read-only**, so we can never corrupt an org graph.
- **Rename is transactional** — the page move and every `[[ref]]`/`#tag` rewrite
  commit all-or-nothing.

## Consequences

- **Easier:** Tine is safe to run on a live, multi-writer graph — the core promise.
- **Harder:** every write path carries baseline checks, flush ordering, and rollback;
  some races can only be *narrowed*, not closed (a non-cooperating external writer in
  the check→rename window). The honest residue is tracked in `DEFERRED.md` with
  severities, not hidden.
- **Committed to:** new write paths must go through the same guards; "it's just a
  rare edge case" is not grounds to skip a data-loss fix.
- **Windows crash-durability limit:** regular-file bytes are still flushed with
  `FlushFileBuffers`, and publication still uses atomic name operations. After
  Tine clones the retained directory capability, validates that exact handle as
  a real directory rather than a reparse point, and retains it across
  publication. No path is opened after validation to re-establish that
  directory. Win32 documents `FlushFileBuffers` for a `GENERIC_WRITE` file
  handle, but does not identify it as an operation that accepts a directory
  handle; requesting `GENERIC_WRITE` on a directory also requests namespace
  mutation rights rather than a narrow durability right. Tine therefore models
  directory-entry flushing as unsupported on Windows only after capability
  clone, metadata, and validation have succeeded. No reopen or flush error is
  whitelisted. Regular-file flush, clone, metadata, validation, publication,
  and all unrelated errors remain fatal. Consequently, flushed file bytes and
  atomic publication are retained, but Tine does not claim that the directory
  entry itself survives a Windows crash.
