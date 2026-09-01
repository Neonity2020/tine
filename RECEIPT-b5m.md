# B5m receipt — media under Managed Storage (manager-completed)

Preserved as `RECEIPT-b5m.md` by the cumulative Wave 1 integration merge.

Provenance: codex gpt-5.6-sol/high lane (dossier
`specs/campaigns/2026-09-invariant-sweep/B5m-media-managed-dossier.md`) was
externally killed AFTER completing implementation and proof logs but BEFORE
writing this receipt. The frontier manager (Claude) reviewed the full diff,
re-ran the unfinished gates, completed the e2e-contracts delta, and wrote
this receipt. Killed-lane artifacts retained in worktree root: b5m-*.log,
baseline-b5m-*.

Contract: assets must be served by the native media protocol under BOTH
storage authorities; containment (traversal/absolute/symlink) and
binding-generation authority refusals unchanged. Defect B026: handler
required `slot.legacy_graph()` (legacy-only gate) → every asset 403 under a
Managed binding.

Pinned base: branch batch/harvest-b5m from master 3f3a7afe (clean before
lane edits).

Owned files: src-tauri/src/media_protocol.rs, src-tauri/src/state.rs,
scripts/e2e-media.mjs, tests/ui-regressions/e2e-contracts.json (manager).

Fix shape: new `GraphSlot::asset_stream_path` resolves per-authority —
Direct via cached legacy graph, Managed via `Graph::open_derived_read_only`
— both delegating containment to the single canonical
`Graph::stream_asset_path`. Unavailable authorities refuse with typed,
scenario-named variants (DirectRetiring, ManagedUnavailable). Handler keeps
stale-binding 403 first and all HTTP mechanics (HEAD/Range/416/MIME/size);
adds debug-gated status diagnostics and a `respond_for_slot` test seam.

Fail-before (lane log b5m-handler-fail-before.log / b5m-fail-before.log):
real-app E2E, Managed binding, `tine-media://` asset → 403; dissolve clause
not triggered. Pass-after (b5m-pass-after.log): same journey → 206 range
serve green, PLUS pinned refusals all green (traversal 404, absolute 404,
outside-symlink 404, stale binding 403, managed-unavailable 403).

Manager-run gates (2026-09-01):
- focused src-tauri: 4/4 green (incl. new
  `asset_stream_authority_names_direct_retiring_and_managed_unavailable`,
  pinned-behavior tests).
- full src-tauri lib suite: 345 passed / 0 failed.
- tine-core suite: red-name set IDENTICAL to 70-name baseline, both
  directions (lane's final-b5m-core.raw.log vs baseline-b5m-names.txt).
- `cargo fmt --all -- --check`: clean.
- e2e-contracts.json: two new blocking rows (Managed serving =
  core-operation; containment/authority refusals =
  exact-safety-interoperability) + one nonRequirement (exact refusal status
  codes); `check-ui-regression-catalog.mjs` green (384 entries).

Regression-catalog disposition: exemption — substitute evidence is the two
blocking e2e-contract rows plus the unit tests above; reason: internal
sweep finding (B026) on the pre-0.7 Managed surface, no GH issue.

Follow-up (recorded, not blocking): the Managed arm pays
`Graph::open_inner` per protocol request (config.edn read+parse, projection
root open, in-process gate registry — no store locks). Bounded but
non-trivial on range-streaming hot paths; candidate: cache a derived
read-only view keyed to binding_generation.

Relaxation ledger: empty.
Verdict: ACCEPTED (manager-verified).
