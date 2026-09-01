# A4-fix receipt — removal of the four run-local identity caps

Preserved as `RECEIPT-a4.md` by the cumulative Wave 1 integration merge.

Provenance: the codex lane for this packet was externally killed before its
first source edit (it had only renamed the repro receipt, preserved here as
`RECEIPT-a4-repro.md`).
The frontier manager (Claude) implemented the fix directly per
`specs/campaigns/2026-09-invariant-sweep/A4-fix-dossier.md`. Repro-phase
findings: `RECEIPT-a4-repro.md` (same directory).

Invariants in play: I-8 (every refusal names an in-scope scenario), I-10 (no
permanently stuck states), I-14 (lifetime-growing cost has a stated bound).

Contract: run-local identity indexes (page names, portable paths, block
claims, Logseq claims) must never refuse for occupancy. The four 4,096-entry
caps named no threat scenario (introduced as "no-store … test index",
commits 95f317e2/f797af50), counted lifetime-DISTINCT identities with no
removal path (permanent across reopen), and the block-claim member refused
only at acceptance — after local_journal_drain published the manifest —
converting a reported save into a permanently OpenRefused store.

Pinned base: c6917539 (repro lane head) on batch/harvest-a4.

Owned files:
- crates/tine-core/src/oplog/page_name_index.rs — cap constant + check removed.
- crates/tine-core/src/oplog/hot_engine.rs — three cap constants + checks
  removed; the dead "insert" instrumentation phase (which timed only the
  removed novel-count/cap check, never an actual insert) removed with it
  (block_claim_insert_nanos field, copy, print); capacity unit test rewritten
  as `block_claim_index_grows_past_any_fixed_capacity`.
- crates/tine-core/src/oplog/hot_engine_integration_tests.rs — a4 region
  flipped from characterization-of-defect to guards-of-fix:
  `a4_run_local_identity_indexes_have_no_fixed_capacity` (source scan: no
  MAX_EPHEMERAL, no "reached its fixed capacity", single
  PageNameTransitionAccess impl; failure messages state the rule and cite
  I-8/I-10 + the dossier), `a4_page_operations_continue_past_the_removed_cap`,
  `a4_reopen_replays_past_the_removed_cap_and_accepts_peer_names` (incl.
  peer-batch acceptance — the pre-fix sync hazard),
  `a4_block_claims_grow_past_the_removed_cap_through_acceptance` (8,192
  lifetime blocks, all accepted, occupancy asserted). Measurement test kept.
- crates/tine-core/src/sync_runtime_tests.rs — stale "REPRO ONLY, no fix"
  header comment updated (the release-only observational probes themselves
  are unchanged and remain env-bounded receipts).
- docs/storage-sync-contract.md — §3 invariant 10 added (no fixed capacity;
  growth driver + archive-rebaselining bound; guard test names). Same change
  set as the code per the living-contract rule.

Exclusions: no retention/release semantics change (released keys stay
retained — rename idempotency and peer validation depend on them); no A5
work; CommittedLocalOverlay page-name field untouched (harmless post-fix;
carded under the acceptance-only refusal census).

Fail-before (necessity gate), run with production files restored to the
pre-fix state and the new guards in place:
- `a4_run_local_identity_indexes_have_no_fixed_capacity` → FAILED (caps present).
- `a4_page_operations_continue_past_the_removed_cap` → FAILED after seeding
  4,096 names (13.3s): the 4,097th create was refused, exactly the repro's
  wedge.

Pass-after (post-fix, same tree):
- fast: a4 source-scan guard + a4 count-semantics + rewritten unit guard —
  all green.
- ignored guards: page-ops, reopen+peer, block-claims (8,192), measurement —
  all green (the 3 release-only application probes correctly refuse to run
  in a debug build; unchanged behavior).
- `cargo fmt --all` (canonical root pass) then `--check` clean.
- Full `cargo test -p tine-core` baseline-by-names vs the 69-name lineage
  baseline (`../baseline-a4-names.txt`): see the comparison block appended
  below after the suite run.

Note on the killed lane's empty `baseline-a4fix-full.txt`: the lane died
before capturing it; the pinned-base baseline used instead is the repro
lane's verbatim 69-name lineage file, which was validated at this exact base
commit in `RECEIPT-a4-repro.md` §10.

## Baseline comparison (appended after the full-suite run)

Full `cargo test -p tine-core` at the fix head: 1727 passed; 70 failed; 50
ignored (98.6s). Set comparison vs the 69-name lineage baseline, both
directions: 0 baseline names disappeared; 1 name appeared —
`sync_runtime::tests::managed_one_block_save_stays_within_two_parser_passes`,
the first of the known load-sensitive flakes already documented in
`RECEIPT-a4-repro.md` §10; serial `--exact --test-threads=1` rerun: ok (0.20s).
Zero new red.

All a4 guards re-run green at the exact post-fmt head (fast pair, the three
past-capacity ignored guards, the measurement test, and the rewritten
`block_claim_index_grows_past_any_fixed_capacity`). The three release-only
application probes correctly refuse to run in a debug build (their guard
message, unchanged behavior).

Verdict: COMPLETE — fail-before proven, pass-after proven, contract updated
in the same commit, zero new red.
