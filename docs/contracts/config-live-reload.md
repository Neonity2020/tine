# Contract — live `logseq/config.edn` reload

What happens when `logseq/config.edn` changes while Tine is running. Kept true
by same-commit updates and by the tests named below.

Before this existed, Tine read the file **once per graph open**. An edit made in
Logseq, in a text editor, or delivered by Syncthing was invisible for the rest of
the session, and the next settings write from Tine was computed from the stale
copy.

## 1. Configuration is not graph text

`logseq/config.edn` is a **plain filesystem file under both storage engines**:
never in `GraphTextScope`, never in the oplog, never projected. That is stated as
a capability contract at `Graph::ensure_config_write_target` and is why settings
are writable even in a read-only managed view.

There is therefore **no Direct-versus-managed fork** in this mechanism. One
file, one parser, one write authority.

## 2. Why the watcher used to drop it

The OS watcher subscribes recursively to each graph root, so the file was always
*watched*. Three filters then discarded it, the decisive one being
`incremental_page_paths`: `.edn` is not an eligible page extension, so an exact
event on it returned "no paths" and the batch forgot it.

`Pending::config_paths` is a separate queue for exactly this reason. The
filename gate (`path_is_config_file_name`) is cheap and rough; the decision is
`tine_core::model::is_config_file_path`, made per graph root when the batch
drains, case-insensitively — a case-folding filesystem may spell it
`Logseq/Config.edn`, and the open path already resolves it that way.

Tested by `watcher::tests::a_config_edn_write_is_queued_even_though_it_is_not_graph_text`
(in-place write, temp+rename, and create — every shape a writer produces) and
`watcher::tests::an_ordinary_page_write_queues_no_configuration_work`.

## 3. What makes it cheap

A refresh discards the entire page cache, and Logseq rewrites `config.edn` on
many ordinary UI actions while Syncthing redelivers it on every peer change. A
byte-identity gate is therefore **mandatory, not an optimization**.

`Graph::open_config_description()` is a digest of the bytes the running instance
was opened with; `model::config_file_description(root)` digests what is on disk
now. A Direct graph refreshes only when they differ — which also means a
settings write Tine performed itself costs nothing here, because the command
already refreshed and the reopened graph matches disk.

`Graph::recent_config_write()` is the second half of the same gate. A setting
Tine writes itself leaves the running graph's *parsed* view stale — it always
has, and `set_favorites` in particular never refreshed — so the open-time digest
alone would read every star toggled in the sidebar as an outside change.
`Graph::write_config` is therefore the single funnel every setter publishes
through, and it records what it wrote.

A managed slot retains no `Graph` to ask, and its refresh is a meta-only reopen
with no cache to lose. The watcher therefore keeps its own last-seen digest per
managed root (`config_seen`) and lets the comparison in §4 decide whether
anything is worth announcing. That memory is not an optional cache: poll mode
cannot name paths and so rechecks every graph every cycle, which without it
would reopen a derived view every three seconds forever. A Direct graph needs no
such memory — its own instance is the witness.

Tested by `config::tests::a_graph_reports_whether_config_edn_moved_since_it_was_opened`,
`config::tests::a_settings_write_tine_performed_itself_does_not_read_as_an_outside_change`
and `config::tests::only_the_graph_s_own_config_edn_is_recognized_as_configuration`.

## 4. What reaches the frontend

`graph-config-changed`, carrying the fresh `GraphMeta` — and **only when the
meta actually moved**. `GraphMeta` derives `PartialEq` for this purpose: a
rewrite that changed no setting Tine surfaces announces nothing.

On the frontend, `graphMeta` is a Solid signal that ~22 modules read reactively,
so most settings update for free (keybindings already re-install on change).
`applyConfigDerivedState` re-applies only the state that is **not** read from
that signal — workflow, journal title format, favorites and the arrangement —
and takes the previous meta so an unrelated settings write does not re-seed
favorites and re-fetch the arrangement page for nothing. A graph open passes
`null`, meaning "apply everything", so there is exactly **one** producer of
config-derived frontend state.

## 5. Refusals and deferrals

| Situation | Behaviour | Why |
|---|---|---|
| Storage transition lane busy | `RefreshOutcome::Deferred`; the window is remembered in `config_recheck` and retried next cycle | Blocking the watcher thread would stall reconciliation for **every** graph behind one graph's load or storage promotion. The file is still on disk, so nothing is lost by waiting |
| Kernel rescan, notify error, or poll mode | Every graph re-checks | Those cycles carry no usable paths; poll mode has none at all. One file read and one digest per graph, against a stat scan already being paid |
| Refresh fails | `graph-watch-error` is emitted | Until it succeeds the window serves stale configuration, which is the failure this whole mechanism exists to prevent. Not silent |
| Journal filename migrations | **Never** run on a refresh | Concord invariant 4: a refresh re-reads configuration, it does not rewrite the tree. An outside config edit must not rename the user's files as a side effect |

## 6. What this does NOT close

`:favorites` is the only list-valued setting Tine writes **wholesale**, from a
list the frontend holds. Live re-reading shrinks the window in which a favorite
added in Logseq is overwritten by the next star toggled in Tine; it does not
close it. Closing it needs a three-way merge inside `set_favorites`' own
`atomic_update` closure, against the disk baseline it already reads.

Every other setter writes a scalar the user just chose, where last-writer-wins
is the expected semantics, and `atomic_update`'s key-local compare-and-swap
already guarantees that an external edit to a *different* key is never lost.
