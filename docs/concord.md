# Concord

<!-- The introduction and the other sections are written by a parallel lane;
     this file currently carries only the "Transport artifacts and VCS
     markers" section. Merge unifies. -->

<!-- BEGIN SECTION: Transport artifacts and VCS markers -->

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

<!-- END SECTION: Transport artifacts and VCS markers -->
