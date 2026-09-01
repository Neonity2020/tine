# Harvest B2 — N-page convergent Direct move

Branch `batch/harvest-b2`, on top of B1 (`5a590368`). Five commits:

| SHA | What |
| --- | --- |
| `444a5045` | the durable recovery record, the native bracket, carry's managed route |
| `d44f3384` | crash matrix, byte-compat fixtures, the living contract, the `classify`/`recover_one` split |
| `04b3b37a` | `src/directMoveOrder.test.ts` — the durable-step order pin |
| `19d72d3f` | real-graph acceptance gate, census deltas, `cargo fmt --all` |
| `0077d148` | the two new commands declared on the managed command surface |

**Invariants in play:** I-3 (one user intent, one operation to storage),
I-2 (every graph write through the audited path), I-4 (Direct layout stays
byte-compatible Logseq), I-20 (via B1's dispatcher, already in the tree).

---

## 1. What actually changed

A Direct cross-page move writes N+1 files. Before B2 a crash between any two of
those writes left the graph **permanently divergent** — the blocks in the
destination *and* still in a source, or (worse) removed from a source and never
written to the destination. B2 makes every such move converge: after any crash
cut, the next open either completes the move or rolls it back.

The mechanism is a **native bracket** around the existing choreography:

1. `begin_direct_cross_page_move` composes and durably commits a recovery record
   naming every participant, **before the first page write**;
2. the frontend runs its unchanged destination-first choreography;
3. `finish_direct_cross_page_move` retires the record once every participant is
   durably terminal;
4. `recover_all` runs at the Tauri pre-open boundary, before
   `open_graph_for_load` serves any page content, and converges anything left.

### Why a bracket and not one native N+1 move command

The dossier's write set limits frontend changes to `src/store.ts` and
`src/carry.ts` through B1's dispatcher arms. A single native "move N pages"
command would have meant rewriting `src/persistence.ts` — the
`holdSourcesForDest` / `releaseSourcesFor` barrier, the base-revision guard, the
conflict epochs, the activation boundary — i.e. replacing the audited Direct
save path rather than bracketing it. The bracket instead lets storage see the
move's **full scope as one declared transaction** (I-3's "the operation must
exist below the UI") while every individual page write remains the exact audited
per-page path (I-2). Nothing about conflict handling, base revisions or
activation changed.

### Where the recovery module lives, and why (dossier asks for this argument)

`crates/tine-core/src/direct_move_recovery.rs`, **not** `src-tauri`.

- It needs `model::atomic_write`, `model::content_rev`, the filesystem-durability
  helpers and the page serializer. Those are `tine-core`.
- It must be testable without a Tauri app, and the crash matrix is 1000 lines of
  test that runs in 0.08s because it does not boot one.
- But `Graph` never holds the app-private root: the store root is passed **per
  call**. Recovery has to run *before a `Graph` exists*, and the spec is explicit
  that `Graph` receives no app-private root.

So the split is: `tine-core` owns the record format, the classification and the
recovery algorithm; `src-tauri` owns the root (`backup::direct_move_recovery_dir`,
keyed by `root_backup_id(root)` under `app_data_dir()`) and the timing
(`graph.rs::prepare_direct_files_open`). That satisfies the spec's location and
timing clauses without putting an app-private path inside `Graph`.

### Recovery writes bypass the conflict machinery, deliberately

`recover_one` writes pages with `model::atomic_write` after checking the file's
bytes against the recorded pre/post images. That is **strictly stronger** than
the base-revision guard it stands in for: the guard asks "is this still the
revision the editor loaded"; this asks "is this still exactly one of the two byte
strings this move accounted for". Anything else is `Diverged` → quarantine.
Contract §4.

---

## 2. Crash matrix (cut point × outcome × test)

Executed by running the **production** durable steps in production order and
stopping after step *k*, then running the real `recover_all` on the resulting
disk. Every reachable disk state is covered *by construction*, not by sampling.
`direct_move_durable_steps` is the single enumeration of those steps, and
`src/directMoveOrder.test.ts` pins that the frontend really emits them in that
order — the matrix is worthless otherwise.

### 1 destination + 2 sources — `every_crash_cut_of_a_two_source_move_converges_on_reopen`

| Cut | Crashed after | `recover_all` outcome | Graph converges to |
| --- | --- | --- | --- |
| 0 | nothing durable | no record to see | pre-move |
| 1 | commit record | `NothingApplied` | pre-move (rolled back) |
| 2 | + write destination | `Completed { pages_written: 2 }` | post-move |
| 3 | + write source 0 | `Completed { pages_written: 1 }` | post-move |
| 4 | + write source 1 | `AlreadyComplete` | post-move |
| 5 | + retire record | no record left | post-move |

"Converges" is the strong form: the whole graph is **byte-equal** either to the
pre-move graph or to the graph an uncrashed move produces. "Some sensible state"
is not the contract. A second `recover_all` is asserted empty at every cut, so
nothing is left to re-apply later.

### The rest of the matrix

| Test | What it cuts / injects |
| --- | --- |
| `every_crash_cut_of_a_single_source_move_converges_on_reopen` | the 1+1 shape (`moveBlock`, `moveBlockFeedNow`, `moveSelectionItems`, one-day carry), cuts 0..4 |
| `a_removal_without_its_addition_is_rolled_back` | the only state that loses blocks: source written, destination not |
| `an_external_write_to_any_participant_quarantines_instead_of_converging` | external-editor / sync / second-instance write, looped over **every** participant |
| `record_composed_after_the_destination_landed_still_completes_forward` | the documented `markDirty` window (§4 below) |
| `a_same_page_move_composes_no_record` | degenerate shape: no record at all |
| `a_journal_and_a_named_page_participate_identically` | feed vs routed page |
| `a_move_into_a_page_with_no_file_rolls_back_by_removing_it` | rollback to "this file did not exist" |
| `duplicate_looking_identities_bind_the_physical_file` | two files claiming one `(kind, name)` — the record binds the path, not the name |
| `every_image_is_durable_before_the_record_names_it` | blob-before-record ordering |
| `malformed_private_state_is_preserved_and_never_applied` | garbage JSON, and a `../outside.md` traversal record |
| `finish_retires_only_a_fully_terminal_move` | `finish` must not force-complete (see §5 — this one found a real bug) |
| `cleanup_is_bounded_and_reclaims_unreferenced_images` | `QUARANTINE_RETENTION = 32` |
| `the_contract_document_states_the_load_bearing_values` | doc-code consistency against `docs/contracts/direct-move-recovery.md` |

### Fail-before / necessity

Kept as a **standing assertion inside the matrix** rather than a one-off run: at
cuts 2–3 the test asserts the mid-crash graph is byte-equal to *neither* terminal
state — i.e. it really is a half-move. That divergent state is exactly what the
pre-B2 tree left behind permanently. If a future change ever makes the mid-crash
state already terminal, the assertion fails and says the matrix has stopped
testing anything. This is stronger than a transcript of one deleted-code run,
which nothing would re-check.

For the order pin: reverting `withDirectMoveRecord` to compose the record *after*
the choreography (record → sources → destination → nothing to retire) turns **6
of 7** `src/directMoveOrder.test.ts` tests red. Restored with
`cp` from a scratch backup, then `git diff --stat src/store.ts` verified empty.

---

## 3. Byte-compat fixtures (I-4)

`both_terminal_states_are_byte_exact_for_every_format_shape` runs six shapes.
For each it asserts three things: the **completed** state is byte-identical to
the uncrashed move's output, the **rolled-back** state is byte-identical to the
pre-move file, and a bystander file in the same graph is untouched.

| Shape | Covers |
| --- | --- |
| `markdown-lf` | baseline |
| `markdown-crlf` | CRLF preservation through a full rewrite |
| `markdown-properties` | `title::` / `alias::` / `tags::` pre-block and its ordering |
| `markdown-headings` | heading shapes |
| `org-lf` | Org |
| `org-crlf` | Org + CRLF |

**The oracle is discharged by reduction, not by a second parser.** The recorded
postimage comes from `serialize_page_dto_for_path` — the *same* serializer the
ordinary Direct save calls. Recovery therefore cannot publish a byte string the
ordinary save would not have written, and the tests assert byte-identity with the
uncrashed move rather than "parses the same", which is strictly stronger than an
oracle round-trip would have been.

---

## 4. Anonymized-graph gate

Run over the real-scale Direct-Files corpus, results **abstract only** — no
corpus content is in any commit, fixture, test or this file.

`crates/tine-core/src/direct_move_recovery_corpus_tests.rs`, `#[ignore]`d, graph
named by `ANON_GRAPH`, copied to scratch, never mutated in place. Every message
and `eprintln!` in it prints counts, indices and booleans only.

```
ANON GATE:       pairs=59  cuts=295  skipped_no_record=0 skipped_empty=3
                 org_pairs=0 crlf_pairs=0 non_byte_exact_save_roundtrip=0
ANON GATE (1+3): triples=11 cuts=77
```

**372 crash cuts on a real graph, all converged**, and at every cut the whole
1046-file corpus was byte-compared: every non-participant file identical.

Three findings, stated abstractly:

1. **The corpus contains no Org page and no CRLF page.** `org_pairs=0`,
   `crlf_pairs=0` over a 59-pair stride sample. Those two byte-compat axes are
   therefore covered **only** by the synthetic fixtures in §3. This is a corpus
   gap, not a defect — worth knowing before anyone treats a green real-graph run
   as covering Org.
2. **Zero pages failed to survive their own save byte-for-byte.** A cross-page
   move rewrites both files wholesale, so a source that merely loses one block is
   fully re-serialized; the probe found no page in the sample where that changed
   a byte it should not have.
3. **Three sampled pages had no movable root block** and were skipped. No
   real-graph shape disagreed with the synthetic corpus, so — per the tiered
   corpus rule — there was nothing to extract into a new fast fixture.

**Write-set extension, argued:** the dossier's write set does not name this file.
I kept the harness as a committed `#[ignore]`d test instead of deleting it,
because the corpus rule says every packet touching storage/save takes the real
graph as an acceptance gate, and a gate that vanishes with its packet has to be
reinvented by the next change. It has zero corpus content and runs only when
`ANON_GRAPH` is set. If the manager disagrees, deleting the file removes it
cleanly (plus 3 lines in `lib.rs`).

---

## 5. A real bug my own test caught

`finish_retires_only_a_fully_terminal_move` failed on first write. Cause:
`retire_if_terminal` called `recover_one`, **which writes**. So
`finish_direct_cross_page_move` would have *force-completed* a move whose source
save was still sitting behind a live conflict banner — silently overwriting a
page the user was being asked about.

Fix: extracted a read-only `classify()` returning
`Result<ParticipantStates, RecordOutcome>`. `recover_one` is now
classify + decide + write; `retire_if_terminal` is classify only, retiring iff
every participant is `Completed`, quarantining on divergence, and writing no
graph byte. Contract §4 states this. It is worth flagging because it is exactly
the class of defect the packet exists to prevent, and it was one function call
away from shipping.

---

## 6. Carry's route, and its fail-before

B1 recorded (and deliberately left) carry's I-6 bug: it ran the Direct
choreography under **every** admission, writing journal files directly beneath a
managed binding. B2 fixes the route.

- `dispatchCarryPersist` is replaced by `dispatchCarry`, a full three-arm front
  door (`direct` / `managed` / `unavailable`); the operation id is now `carry`.
- `carryDay` and `carryDaysBack` wrap their **whole body** in it, so the refusal
  is taken *before* `carryUnfinished` touches memory — the editor is never left
  holding a mutation storage never accepted.
- The managed arm raises `MANAGED_MULTI_SOURCE_MOVE_UNAVAILABLE_TOAST` at
  severity `error`, the same constant `moveBlocksRelative`'s managed arm now
  uses (it was a duplicated literal; there is one now, I-12).
- **No managed carry arm was implemented.** The multi-source lift is an undecided
  product question and stays that way.
- B1's two "carry has no managed arm" assertions were **deleted deliberately**,
  as B1's receipt intended, and replaced with their opposites in
  `src/storageDispatch.test.ts` and `src/storageDispatchRoutes.test.ts`.

**Fail-before (harm level, not counter level).** I surgically reverted `carryDay`
to the unconditional Direct call and ran a throwaway harm-only test:

```
expect(save, "I-6: carry wrote Direct under a managed binding").not.toHaveBeenCalled();
  →  Number of calls: 1        (savePage called with today's journal DTO)
```

After restoring `src/carry.ts` (verified by an empty `git diff --stat`), the same
test passes. The throwaway file was deleted; the permanent assertions are the two
route tests.

**One decision for Martin (dossier default followed).** The dossier says the
managed carry refusal uses "the same toast/severity as the other multi-source
refusal", so it reuses that string verbatim: *"Managed cross-page moves currently
require all selected roots to share one source page."* For an **N-day** carry
that is accurate. For a **single-day** `carryDay` it is a slight mismatch — the
real reason is "there is no managed carry at all", not "too many source pages".
I followed the dossier rather than inventing a second string. If you want a
carry-specific message, it is one constant in `src/carry.ts`.

---

## 7. Contract, and the refusal table (I-8)

`docs/contracts/direct-move-recovery.md` (new, 205 lines) is the living contract:
§1 why, §2 store location and record layout, §3 the four durable steps and their
timing, §4 the state machine + refusal table, §5 bounded cleanup, §6
byte-exactness, §7 managed storage and carry.
`the_contract_document_states_the_load_bearing_values` is the doc-code
consistency test (the four step phrases, `QUARANTINE_RETENTION = 32`, schema 1).

**Trust boundary stated in the contract:** the 2026-08-07 decision. No defence
against a byte-forging local attacker; crash/power loss, torn write, disk error,
external-editor race, sync delivery and honest concurrent instances *are* in
scope.

| Site | Refusal | In-scope scenario |
| --- | --- | --- |
| `recover_one`, participant `Diverged` | quarantine; write nothing; both versions preserved; the ordinary conflict machinery surfaces it on the next save | external-editor race, sync-service delivery, honest concurrent instance |
| `DirectMoveRecord::validate`, path not contained | record quarantined unread | crash / torn write / disk error leaving app-private state malformed. Corruption containment, explicitly **not** an anti-attacker check |
| `RecoveryStore::read_blob`, digest mismatch | record left in place, reported failed | torn write, disk error |
| `pending()`, undecodable or wrong-schema record | quarantined, never applied, open proceeds | unrecognized private state (I-7): preserved as backup, graph rebuilt from files |

**No path here refuses a move.** If the record cannot be composed — no app-data
home, unreadable file, serializer refusal — `begin` returns `null`, a diagnostic
is written, and the move proceeds *unbracketed*, exactly as convergent as it was
before this packet. Refusing to move a page because device-private state is
unavailable would be an availability bug with no in-scope threat behind it.
`src/directMoveOrder.test.ts` pins that behaviour ("a record that could not be
composed … does not refuse the move").

**Contract delta:** the new contract doc. No change to
`docs/storage-sync-contract.md` — nothing here touches managed storage's layout,
state machines or schema versions, and the one managed-facing change (carry's
refusal) is a frontend route, recorded in this contract's §7.

### The one documented benign window

`markDirty(dest)` stays **synchronous**, before the record round-trip, so
`flushAll` on graph switch or window close can never miss the destination. The
debounce can therefore publish the destination before the record exists. Every
outcome still converges: recovery then sees an already-terminal destination and
carries the move **forward**, which is the safe direction. Proven by
`record_composed_after_the_destination_landed_still_completes_forward`. It is in
the contract (§3), not only in a comment.

---

## 8. Write set — what I touched, and the extensions

Inside the dossier's write set:

- `crates/tine-core/src/direct_move_recovery.rs` (new, the record + recovery)
- `crates/tine-core/src/model.rs` (+107: `Graph::prepare_direct_cross_page_move`)
- `src-tauri/src/graph.rs` (pre-open recovery + the two commands)
- `src/store.ts`, `src/carry.ts`, `src/storageDispatch.ts` (via B1's arms)

Extensions, each mechanically forced:

| File | Why it had to change |
| --- | --- |
| `src-tauri/src/backup.rs` | it already owns `root_backup_id`; the graph-keyed app-private root belongs beside it |
| `src-tauri/src/lib.rs` | `generate_handler!` + the `use graph::{…}` list, or `backend_command_parity` fails |
| `src/backend.ts`, `src/mock.ts` | a command the frontend calls needs an interface arm and a mock arm |
| `crates/tine-core/src/projection_producer_census.rs` | its own guards failed (§9) |
| `src-tauri/src/managed_command_surface.rs` | its own guards failed (§9) |
| `crates/tine-core/src/direct_move_recovery_corpus_tests.rs` | the anonymized gate (§4, argued) |

Untouched, as instructed: `hot_engine.rs`, `query.rs`, `sync_runtime.rs`,
`oplog/wire.rs`, `sqlite.rs`.

---

## 9. Guards that fired, and what I did about them

Three separate house guards caught things I had not thought about. All three are
the good kind of failure.

1. **`projection_producer_census::g_a`** — the new module's raw filesystem
   primitives were unpinned. Added three rows for
   `direct_move_recovery.rs` (`fs.create_dir_all` 5, `fs.remove_file` 4,
   `fs.rename` 1) with a comment explaining that its **graph** writes are absent
   from G-A precisely because they are not raw primitives.
2. **`projection_producer_census::g_b`** — `atomic_write` callers 6 → 10.
   That is the point: recovery publishes graph bytes through the same named
   audited protocol an ordinary save uses. Also `commit` 6 → 7, which I did
   **not** accept: I renamed `RecoveryStore::commit` to `commit_record`, because
   `commit` is a tracked choke-helper name and a second unrelated one would have
   silently inflated a load-bearing count and made the census lie. The reason is
   in the method's doc comment so the next person does not undo it.
3. **`managed_command_surface`** — the two new commands were undeclared. This is
   not merely documentation: `is_known_command` is the flight recorder's
   allowlist, so their diagnostics would have been dropped. Both are declared
   `LegacyOnly` with their refusal reasons in `REFUSED_UNDER_MANAGED_STORAGE`.

---

## 10. Gates

| Gate | Baseline (branch start) | Final |
| --- | --- | --- |
| `npx tsc --noEmit` | 0 errors | **0 errors** |
| `npm test` | pass | **179 files, 1462 tests, pass** |
| `cargo test -p tine-core --lib` | 70 failing names → `../baseline-b2-names.txt` | **1740 passed, 70 failed — the identical name set** |
| `cargo test -p tine --features custom-protocol` | not baselined (dossier asked only for tine-core) | **342 passed, 0 failed** |
| `cargo fmt --all` | — | clean, run from repo root, only my lines moved |

**tine-core delta by NAMES.** A raw run showed 75 failing names. Set-compare
against the baseline gave five new names and zero disappearing:

- `projection_producer_census::g_a_…` and `g_b_…` — **real**, mine, fixed in §9.
- `sync_runtime::tests::managed_search_reports_building_then_returns_backend_results`
- `sync_runtime::tests::over_limit_restore_rediffs_after_interference_and_resumes_from_durable_cursor`
- `sync_runtime::tests::projection_recovery_equivalence_oracle_real_store_subset`

The three `sync_runtime` names are the load-sensitive flakes the dossier warns
about (two of them by name). Re-run serially with `--test-threads=1` on the exact
head: **all three pass**. `sync_runtime.rs` is outside my write set and I did not
touch it.

**Net: zero new red names.** Final confirmation run on the exact head
`0077d148`: `1740 passed; 70 failed`, and a set-compare of the failing names
against `../baseline-b2-names.txt` is empty in **both** directions — nothing new
appeared and nothing silently disappeared.

### E2E

| Journey | Contract stability | Result |
| --- | --- | --- |
| `scripts/e2e-blockselect.mjs` (dossier-named) | stable | **PASS**, exit 0 |
| `scripts/e2e-selection-actions.mjs` — the actual cross-page move + drag + durable relaunch journey | **quarantined** since 2026-08-17 (harness debt) | **PASS** |

Both were run against a release binary rebuilt from the exact final head
`0077d148` (`npm run build` + `cargo build --release --features custom-protocol`),
not from an earlier commit.

The move journey is quarantined, so it is not a blocking gate, but it is the
best real-app evidence available for this packet and it passes: selection-owned
H2, one undo, a real pointer bullet drag across the boundary, and a **durable
native relaunch** — i.e. the move survived to disk with the new
begin/finish bracket in the binary. Note for whoever un-quarantines it: it does
not self-provision, and needs `TINE_APP=<binary>` and
`TINE_CANDIDATE_COMMIT=<40-char sha>` explicitly; without them it exits 1 before
starting. That looks like the harness debt the quarantine note refers to.

**No E2E assertion was relaxed, deleted or quarantined by this packet.**
Relaxation ledger: empty.

**Observation-boundary honesty.** The recovery itself is proven at the layer
where it actually runs — the `tine-core` storage boundary, before any UI exists —
by driving the production functions in production order. The frontend's half (the
emission order) is proven at the store boundary. There is **no** native E2E that
kills the process mid-move; writing one means a new blocking journey plus a
burn-in, and it would have to compute the app-private store path itself. That gap
is recorded rather than papered over.

---

## 11. Deferred, with reasons

| Item | Why |
| --- | --- |
| A native E2E that crashes the app mid-move | new blocking journey ⇒ unchanged-binary burn-in required; the storage boundary is the layer the recovery actually runs at, and it is fully covered. Recorded as the honest gap above. |
| A managed carry arm | undecided product question; dossier says do not implement. Nothing in my reading makes the multi-source lift trivially safe: the native move accepts one source page and a carry is N. |
| Org / CRLF coverage on the real corpus | the corpus contains neither. Synthetic fixtures cover both. Stated as a corpus gap in §4, not silently. |
| `cargo test -p tine` baseline | the dossier's baseline instruction named `tine-core` only; the crate is fully green now (342/342), so the absence of a baseline costs nothing. |
| A merge of `origin/master` | `origin/master` has not moved since the branch point (`git rev-list --count HEAD..origin/master` = 0). |

---

## 12. Commands run

```
source scripts/env.sh
npx tsc --noEmit
npm test
cargo test -p tine-core --lib                       # baseline + final, set-compared by name
cargo test -p tine-core --lib -- --test-threads=1 <the three flakes>
cargo test -p tine --features custom-protocol
ANON_GRAPH=<graph> cargo test -p tine-core --lib direct_move_recovery_corpus -- --ignored --nocapture
npx vitest run src/directMoveOrder.test.ts
npx vitest run src/storageDispatch.test.ts src/storageDispatchRoutes.test.ts
npm run build && cargo build --release --features custom-protocol
node scripts/e2e-blockselect.mjs
TINE_APP=$PWD/target/release/tine TINE_CANDIDATE_COMMIT=$(git rev-parse HEAD) \
  node scripts/e2e-selection-actions.mjs
cargo fmt --all
```

Not pushed. Staged with explicit paths only; no `git add -A`, no tree-wide
clean/reset/stash. `~/research/brain` was never read, searched or referenced.
No content from the anonymized corpus appears in any commit, source file,
fixture or in this receipt.
