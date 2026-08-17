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

## Transport artifacts and VCS markers

Tine does not sync your Direct Files graph itself — you bring the transport
(Syncthing, Dropbox, Seafile, git, Fossil, a USB stick). When a transport
cannot merge two versions of a file it leaves an artifact behind, in its own
format. Tine recognizes those artifacts, keeps them from polluting your page
list, and never rewrites or renames them.

### Conflict copies (sync tools)

When the same page was edited on two devices, file-sync tools keep one version
under the page's name and write the other as a renamed *conflict copy*. Tine
recognizes the exact naming shapes these tools generate:

| Tool | Generated name |
| --- | --- |
| Syncthing | `Page.sync-conflict-YYYYMMDD-HHMMSS-DEVICEID.md` (the device id is up to 7 characters `A–Z`/`2–7`; very old Syncthing versions omitted it) |
| Dropbox | `Page (conflicted copy …).md` / `Page (Alice's conflicted copy …).md` |
| Seafile | `Page (SFConflict user@example.com 2026-08-01-10-00-00).md` |

What happens to a recognized conflict copy:

- It is **not indexed as a page** — it never appears in the page list, search,
  or autocomplete, so it cannot duplicate the real page (or hijack the page's
  identity through a `title::` property).
- It is surfaced in **Settings → Backups & recovery → Sync conflict copies**,
  where you can review a block-by-block diff against the current page and
  merge either side (or both) per block, or discard the copy (recoverable from
  trash).
- Tine never renames, rewrites, or deletes a conflict copy on its own.

Only the exact generated shapes above are treated as conflict copies. A real
page whose name merely resembles one — say `Foo.sync-conflict-notes.md` —
stays an ordinary page.

Deliberately **not** recognized, because their names are indistinguishable
from ordinary page names:

- **OneDrive** conflict copies (`Page-COMPUTERNAME.md`): no sentinel word, no
  timestamp — any page name with a dash suffix would false-positive and be
  hidden from your graph.
- **Google Drive** duplicates (`Page (1).md`): the same suffix Drive uses for
  ordinary duplicate uploads.

Copies from these tools appear as ordinary (duplicate) pages; merge them by
hand.

### VCS merge conflict markers (git, Fossil)

When a `git merge`/`git pull` (or Fossil update/merge) cannot reconcile two
versions of a file, the VCS writes both versions *into the file*, separated by
column-0 marker lines — `<<<<<<<`, `|||||||`, `=======`, `>>>>>>>` (git), or
Fossil's verbose `<<<<<<< BEGIN MERGE CONFLICT …` / `####### SUGGESTED
CONFLICT RESOLUTION …` / `>>>>>>> END MERGE CONFLICT …` lines. The VCS then
relies on finding those exact markers, at column 0, to know the conflict is
still unresolved.

An outliner that re-saves such a file destroys that: the markers are not
bullet lines, so a normal save re-indents them as continuation text (or drops
them), git stops recognizing the conflict, and one side of the merge can be
lost without anyone choosing it.

Tine therefore **quarantines** marker-bearing files:

- The page **stays readable** — Tine renders the file as-is, markers and all,
  with a banner explaining the state.
- **Every save to it is refused** (including force-save) with a message naming
  the markers found. Tine never rewrites a file that carries unresolved merge
  markers.
- Affected files are listed in **Settings → Backups & recovery → VCS merge
  conflicts**.
- You resolve the merge where it belongs — in your VCS or an external editor.
  As soon as the markers are gone from disk, the page becomes editable again
  automatically; nothing needs to be reset in Tine.

A page that merely *documents* merge conflicts is not quarantined: markers
quoted inside code fences (or indented inside a bullet) don't count, and a
lone `=======` divider line never triggers the quarantine on its own.

In-Tine resolution of these conflicts (choosing sides block-by-block inside
the page) is planned for a later Concord phase; the quarantine guarantees that
nothing is lost in the meantime.

## The base ledger

Tine remembers, for every page, **the last version it agreed on with the
disk** — the text it last read from or wrote to the file. That remembered
version is the common ancestor of any later divergence, which is what turns a
conflict from a guessing game into a mostly-answered question: comparing each
side against the ancestor tells Tine *who changed what*.

When you review a sync conflict copy (Settings → Backups & recovery → Sync
conflict copies → *Review & merge*), Tine uses that ancestor when it has one:

- A block only **you** changed arrives with *your* version pre-selected.
- A block only the **other device** changed arrives with *its* version
  pre-selected.
- A block **both** sides changed is a real conflict — no pre-selection; you
  decide.

Pre-selected rows are labeled *suggested*, and the toolbar says when
suggestions are in play. Nothing is ever merged automatically — you review the
rows and click merge, exactly as before; the suggestions just mean the common
case is glance-and-confirm instead of hunt-and-compare.

Practical notes:

- The ledger lives in Tine's app data folder (`concord-ledger/`), **outside
  your graph** — your sync tool never sees it, and it never touches your
  files.
- **Deleting it is always safe.** Tine falls back to the plain two-column diff
  (no suggestions) until the ledger repopulates through normal editing.
- It fills in as you work: pages saved or reloaded since the feature arrived
  have a remembered version; untouched pages simply have none yet.

