# Contract — live `logseq/config.edn` reload

What happens when `logseq/config.edn` changes while Tine is running. Kept true
by same-commit updates and by the tests named below.

Before this existed, Tine read the file **once per graph open**. An edit made in
Logseq, in a text editor, or delivered by Syncthing was invisible for the rest of
the session, and the next settings write from Tine was computed from the stale
copy.

## 1. Configuration is not graph text

`logseq/config.edn` is a **plain filesystem file**: never in `GraphTextScope`
and never saved through the page path. That is stated as a capability contract
at `Graph::ensure_config_write_target`.

The mechanism therefore has one file, one parser, and one write authority.

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

`Graph::served_config_description()` is a digest of the bytes the served
configuration was taken from: the bytes the instance was opened with, then
whatever `Graph::take_in_config` last took in. `model::config_file_description(root)`
digests what is on disk now. The watcher does nothing when they are equal, and
otherwise asks `take_in_config`, whose `ConfigReach` decides: `Unchanged` and
`Settings` are taken in by the running graph, `Graph` reopens it.

`Graph::write_config` is therefore the single funnel every setter publishes
through, and it takes in what it wrote, so a star toggled in the sidebar costs
no reopen. A change that reaches the graph is never taken in, so it leaves the
digest behind: an outside change folded into Tine's own read-modify-write still
reopens, and an outside revert to the opening bytes still reads as a change.
(Two earlier digests — the open-time bytes and the last bytes written — each
missed one of those.)

Tested by `config::tests::a_graph_reports_whether_config_edn_moved_since_it_was_opened`,
`config::tests::the_watcher_gate_matches_disk_only_when_disk_was_taken_in`
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
