# Index readiness and liveness

What the search index (the Direct Files SQLite projection) promises the
surfaces that read it, and in how long. It exists because the index is the
only answer source for queries and, for narrowing, reference panels: a single
source is sound only when it is either ready or visibly failed within a bound
(GH #594; design note `2026-09-24-index-liveness-design`, approved by Martin
2026-09-24). Correctness of what the index says is covered elsewhere; this
contract is about how long a state may last and what readers are told.

Tests pin the values below: `index_readiness_contract_matches_the_code` in
`crates/tine-core/src/model_gh594_liveness_tests.rs`.

## States

One function, `direct_projection_owner.rs::index_state`, computes the index's
state under the `pending` lock. Both the progress readers are told
(`DirectProjection::progress_at`) and "is index work coming", which readers wait
on (`index_work_coming`), are read from it; there is no second producer (L2).

| State | Meaning | Reader is told |
| --- | --- | --- |
| Ready | the image answers for the current generation | the answer |
| Working(reason) | queued or running work will change the image | `not-ready:<reason>`, retried |
| Failed(class) | the index stopped trying this session | `unavailable:index_failed` with the class |
| Stopped | the worker is closed or waits for another process's writer lease | `unavailable:projection_unavailable` |
| Stale | nothing is queued and the image is not current | one repair, then a retry |

## L1 · Bounded non-readiness

A build or update that fails is retried at once, then after 1 s; the
`INDEX_ATTEMPTS` = 3rd consecutive failure leaves the index **Failed** with the
class of that failure. A success resets the count. A worker that cannot set up
its image (its directory or its stage cleanup fails) is Failed at once.

Failed is terminal for the session. Only the user's **Retry** (the
`retry_index` command, which reopens the graph) or the next launch builds the
index again; there is no automatic retry after Failed (Martin, 2026-09-24).
While Failed the worker takes no work: edits queued meanwhile wait for the
retry.

## L3 · Every reader wait is bounded

- A derived read (page list, aliases, block-ref counts, block resolution, ...)
  waits for index work that is coming at most `DERIVED_READ_PATIENCE` = 60 s,
  shared by every wait inside it (`derived_reads.rs::ReadDeadline`). After that
  it answers the way it answers when nothing is coming: from the parsed pages.
- When the index is Ready, a reference panel parses only the candidate pages
  the index names. A candidate the index still lists but that is gone from
  disk or cannot be parsed is skipped, not a reason to walk every page:
  candidates are a superset hint. A source whose indexed revision is checked
  still declines on a mismatch (`parse_pages_on_demand_inner`).
- A reference panel (Linked or Unlinked References) first asks
  `Graph::reference_readiness`, which waits at most the 250 ms reference wait:
  while the index is Working it is told `not-ready` at once, and once Failed it
  is told `index_failed`. It no longer waits for its alias lookups first.
- Rename and delete read the page list before they take the graph-text
  identity lock, so the scope check under the lock reads the memo instead of
  holding every save while it waits.

## L4 · The user always sees the state

Linked References, Unlinked References and query blocks show one of: an
answer; "indexing…" / "rebuilding the index…" while not ready; or, once Failed,
the failure's code with **Retry** and **Create diagnostic report**
(`src/components/IndexFailedNotice.tsx`). A failed index is shown as failed:
there is no page-scan fallback for queries or references (D-10). Search keeps
answering from an already-parsed page cache, as it does for any unavailable
index.

## L5 · Every failure is observable

Each failed attempt reaches the flight recorder as `index.failure` with its
class, attempt number and whether it was terminal
(`docs/contracts/diagnostics.md`). The classes, `IndexFailureClass::as_str`:

| Code | Cause |
| --- | --- |
| `file_in_use` | another process holds an index file (Windows sharing violation) |
| `disk_full` | no space for the index |
| `permission_denied` | the index directory or a file in it cannot be written |
| `io` | another operating-system I/O error |
| `out_of_memory` | memory ran out while building |
| `busy` | the database stayed locked |
| `corrupt` | the stored index is damaged |
| `constraint` | the index refused rows Tine wrote (a Tine defect) |
| `pages_kept_changing` | the graph changed under every attempt to read it whole |
| `graph_unreadable` | the graph's pages could not be listed |
| `writer_refused` | the graph refused the index's writer |
| `no_progress` | an attempt ended with nothing done |
| `other` | none of the above |

## L7 · The integrity check never delays an answer

Opening the stored index checks only its schema. Its integrity
(`PRAGMA quick_check`, 1.2–1.7 s on a 10k-page graph) is checked in the
background, on its own read-only connection, and only when it is owed
(`direct_projection/integrity.rs`):

- **Threat it defends against:** power loss or an OS crash on storage that
  does not honour fsync, and disk errors. The index is SQLite in WAL mode at
  `synchronous=NORMAL`, so a killed process (Android's memory and power
  management, a crash, a force-stop) cannot damage it and never causes a
  check or a rebuild.
- **Owed when:** no record of a passed check exists beside the index (in the
  app's data directory, never the graph), the OS has rebooted since the last
  pass, or the last pass is older than `CHECK_INTERVAL` = 7 days. A fresh
  build checks the image it publishes and records the pass.
- **While it runs:** nothing waits for it. Closing the graph, a config change
  or a replacement image interrupts it and waits for it to let go of the file.
- **Damage found:** reported as `index.failure` with class `corrupt` and
  attempt 0, and the index is rebuilt, as for damage a read meets.

## L8 · Launch: stored answers

A reopen does not make display surfaces wait for the launch check (the survey
that compares the graph on disk with the stored index; up to seconds on a 10k
page graph, longer on Android). While it runs, the index answers display reads
from the image the last session left (design note
`2026-09-24-launch-serve-stored-design`, approved by Martin 2026-09-24).

- **Currency.** Every index read names a `Currency`: `Current` (ready at the
  exact generation, as before) or `LaunchStored` (`Current`, or the stored
  image while `serving_stored`). A read inside `Graph::display_read` is
  `LaunchStored`; every other read is `Current` (`Graph::read_currency`).
  Query snapshots (Gate Q), the reference-panel readiness check and every
  reader wait take the reader's currency.
- **Served when** (`direct_projection_owner.rs::serving_stored`, under the
  `pending` lock): the image was opened under this facts version and parse
  configuration, it is not yet validated, no fresh build is owed or running,
  the worker is up and not failed, and no edit of this session is waiting to be
  applied. The last clause is read-your-writes: an edit made during the check
  is applied to the image before it serves again. A configuration or facts
  version changed while closed serves nothing until the fresh build.
- **Acting reads stay current.** A display read that acts on its answer runs
  its index read through `Graph::exact_read` and waits for the check:

  | Read | Why it must be current |
  | --- | --- |
  | `try_list_pages` | decides a rename/delete refusal |
  | `templates` | inserts the template's text into a page |
  | `indexed_creation_evidence` | decides whether a page name already exists |

  Queries outside a display read (export, plugins) are `Current` too.
- **Never memoized.** An answer served from the stored image marks its thread
  (`Graph::note_stored_served`); `answer_is_complete` is then false and no memo
  keeps it, so nothing outlives the check.
- **Asked at open, corrected when the check lands.** No frontend read waits
  for `warm-cache-done` before asking; the backend decides whether to serve or
  wait. On `warm-cache-done`, `correctLaunchAnswers` (`src/ui.ts`, wired in
  `src/App.tsx`) re-asks Ctrl-K, the reference panels, query blocks, the page
  list, the navigation index (aliases, page identities), referenced names and
  block-ref counts, so an edit made while Tine was closed appears as soon as
  the check applies it.

## L6 · Liveness is tested

`model_gh594_liveness_tests.rs` pins: a build that always fails ends Failed
with its class within the attempts and the panels say so, and a reopen then
builds it; a derived read answers within its patience while announced work
never arrives; a reference panel asked while indexing is told at once; a
candidate page that is unparseable or deleted outside Tine parses no other
page (`gh594_an_unhydratable_candidate_does_not_parse_the_graph`).
`model_launch_serve_tests.rs` pins L8: a clean reopen answers display reads
during the check with no parse, acting reads wait, an edit made while closed
is corrected when the check lands, an edit during the check is read back, and
a changed configuration serves nothing stored.
