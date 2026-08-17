# 0056. Concord base ledger enables block-level 3-way conflict suggestions

- **Status:** Accepted
- **Date:** 2026-08-17

## Context

Concord (GH #337, spec L2) rests on one structural advantage no plain-file
competitor has: Tine can always know the last text it read from or wrote to
disk per page — a true common ancestor for every "Tine edited + disk changed"
divergence. Until now no such store existed: backups are launch-only snapshots
without a per-file API, and managed storage's projection bases are intent-scoped
and cleaned up on completion. The shipped conflict engine (`sync_diff.rs`,
ADR 0020) is therefore 2-way: it can show *that* two versions differ per block,
but never *which side changed*, so the merge UI defaults every row to
keep-mine and the user must reason out each hunk alone.

ADR 0020 explicitly rejected 3-way merge. Its objection was specific: a
text-blob 3-way (diff3 over lines) mangles block nesting and cannot express
per-block keep-both, and *auto*-merging was rejected outright as a data-safety
hazard. Those objections target line-oriented merge mechanics and silent
application — not the presence of a base per se.

There is also a timing trap discovered during design: the natural update rule
("record what was read/written") destroys the base exactly when it is needed.
When a sync tool replaces the winner file and drops a conflict copy, the
watcher admits the winner's new bytes — overwriting the stored base with
content identical to one side, which would make a 3-way collapse into
"take the other side everywhere".

## Decision

We will keep, per graph-relative page path, the last content Tine successfully
read from or wrote to disk — the **Concord base ledger** — and use it to
upgrade the conflict diff from 2-way to **block-level 3-way with per-row
suggestions that are only ever pre-selected, never auto-applied**.

1. **The ledger is a disposable cache outside the sync tree.**
   `<app_data>/concord-ledger/<root-id>/` (backups' root-id convention):
   content-addressed blobs keyed by sha256 plus a small path→hash index.
   Updates happen after a successful save commit (`commit_write`) and after an
   external-change admission (`sync_file_content`), enqueued to one background
   worker — the foreground cost is a channel send. Reads verify the blob hash;
   missing/corrupt/stale state degrades to today's 2-way diff. It never blocks
   open, save, or reload, and errors are logged, not surfaced. Attached only
   for Direct Files graphs; managed storage keeps its own authority chain.
2. **3-way stays block-structural.** `diff3_docs(base, mine, theirs)` reuses
   the exact mine/theirs alignment of ADR 0020's engine (row ids stay valid
   for `merge_blocks`), then classifies each row against the base:
   mine-only change → suggest mine; theirs-only → suggest theirs; both
   changed → true conflict, no suggestion. A path-free seam
   (`diff3_texts` / Tauri `text_block_diff3`, plus 2-way `text_block_diff`)
   is a pure function of the texts for the future in-page UI (P4).
3. **Suggestions are advice.** The existing `SyncConflictMergeModal`
   pre-selects the suggested side per row and labels it "suggested"; the merge
   still applies only on the user's explicit confirm, through the unchanged
   ADR 0020 resolve path (page lock, `base_rev` guard, org firewall,
   stage-before-commit).
4. **Conflict-copy pinning defeats the timing trap.** When a conflict copy is
   first observed, the winner's then-current ledger entry is pinned under the
   copy's identity (first-wins; dropped when the copy is resolved or
   discarded). The conflict diff prefers the pin; and a base byte-identical to
   the winner is treated as the admission artifact and falls back to 2-way
   rather than blanket-suggesting "theirs".

### Relation to ADR 0020

This ADR answers 0020's rejection rather than ignoring it. 0020 rejected a
*text-blob* 3-way because line diffs mangle block trees — the 3-way here is
computed on the same block-tree alignment 0020 itself introduced, so nesting
and keep-both semantics are untouched. 0020 rejected *auto-merging* — nothing
here auto-applies; the base only chooses which side of an already-rendered row
arrives pre-selected, and the resolve path, its guards, and its recoverability
guarantees are exactly 0020's. The diff/merge symmetry invariant (one
alignment intermediate) is preserved because the 3-way annotates the alignment
instead of replacing it.

## Consequences

- Divergences with a known base become glance-and-confirm: non-overlapping
  changes arrive with the right side already selected, and only genuinely
  contested rows demand thought. This is the substrate for P4's in-page
  resolution and suggested-resolution UX (Fossil's idea).
- A new app-private store grows with edit volume between opens; a prune at
  graph open drops blobs unreferenced by index and pins. Deleting the whole
  directory is always safe — behavior degrades to 2-way until it repopulates.
- The base is only as good as the ledger's history on this device: a fresh
  install, a cleared cache, or a page never touched since install yields no
  base (2-way fallback), and a base can be stale after long offline periods —
  the both-changed verdict then honestly reports "conflict" instead of
  guessing.
- The known residual gap: on the device that itself edited the page, the
  ledger's last-agreed text equals that device's own last save, not the true
  common ancestor — its pin then yields base == theirs, whose suggestions
  (keep mine everywhere) match today's safe default. Only a device holding the
  genuine ancestor gains full 3-way power. Cross-device base exchange is
  explicitly out of scope (that is managed storage's territory, invariant 6).
- `sync_file_checked` and `commit_write` each carry one small hook; the ledger
  machinery lives in `concord_ledger.rs` and can be deleted wholesale without
  touching the save or reload paths.
