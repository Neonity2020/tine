# Concord — Tine alongside your other tools

**Concord** is Tine's contract for living in a graph that other software also
touches — an external editor, Syncthing, Dropbox, git, Fossil, a USB stick.
Tine does not bring its own sync; you bring any transport you like, and Concord
is Tine's side of the bargain: external disk changes appear in Tine as fast as
in a code editor; nothing either side wrote is ever silently lost; whatever
genuinely conflicts becomes a calm, block-level review resolved in place inside
the page — and the files on disk stay plain, valid Logseq Markdown throughout,
with zero invented markers or metadata. Concord is being delivered in phases;
this document describes what is implemented today and grows with each phase.

## Freshness

### Clean pages reload automatically

When a file changes on disk and its page has no unsaved edits in Tine, the
loaded page is replaced with the disk version silently — no dialog, no page
flicker, no action needed. The file watcher notices the change (default:
a real OS watcher; a 3-second polling scan on filesystems where that is
unreliable — see the `watch_mode` setting), re-reads just the changed files,
and refreshes any pane that shows them. Tine's own saves are recognized and do
not bounce back as external changes.

A page with **unsaved edits** is never clobbered by a disk change. Tine proves
per page that the file really diverged from what your editor started from; a
real divergence surfaces the conflict banner where you choose which side wins,
and both sides remain recoverable.

### While you are editing: deferred, not dropped

If a file changes on disk while you are *actively editing* that very page —
caret in a block, a block move mid-drag, a page rename in progress — Tine does
not yank the page out from under you. The reload is **deferred**: Tine records
it and replays it automatically the moment the blocking state clears (you click
away, the move settles, the rename finishes). At replay time the safety checks
run again from scratch — if you typed into the page in the meantime, the reload
is not applied; the normal conflict protocol takes over so your keystrokes are
never silently overwritten.

### Reporting slow or missed external changes

An external change should show up in Tine well under a second with the OS
watcher (a few seconds in `poll` mode). If you see multi-second delays or
changes that never arrive, Tine keeps **latency receipts** you can attach to a
bug report — always on, with no setup:

1. Reproduce the slow external change once or twice.
2. Open the devtools console (right-click → *Inspect Element* → *Console*).
3. Run:

   ```js
   await __tineWatcherLatency()
   ```

4. Copy the output into your report.

The result is the last 64 external-change batches, oldest first. For each
batch: which graph, the watch mode (`inotify`/`poll`), how many pages changed,
whether a full rescan was forced, how many reconcile errors occurred, and the
per-stage timings — `event_to_reconcile_ms` (OS callback to the start of
re-reading, including the coalescing window), `reconcile_ms` (re-read, parse,
and notify the UI), and `event_to_emit_ms` (total, callback to UI). The same
receipt is also written as a `watcher-latency` line to Tine's log output; if
you run with `TINE_DEBUG=1` (see README → Troubleshooting) it is captured in
`tine-debug.log`, which you can send instead. Receipts contain file counts and
timings only — never note content.
