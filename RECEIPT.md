# B5s receipt — audited session persistence

## Contract and verdict

PASS for Harvest packet B5s at pinned base
`2dcd01341e6797f1223d35bc2d1fdd7a4f98f910`.

The app-private session saver now follows a named I-1/I-2 protocol: one
`SESSION_LOCK` serializes tab-action bursts, while
`tine_core::model::atomic_write` supplies a unique create-new temp, complete
file barrier, atomic replacement, failed-temp cleanup, and the strict
directory-barrier error policy. The one-time legacy session migration is
under the same lock and pairs its rename with
`tine_core::model::sync_dir_for_rename`, so real durability failures are
reported rather than falsely acknowledged.

The existing hand-rolled `atomic_write_workspaces` protocol collapsed
trivially into the same core primitive. The production writer census in
`settings.rs` is therefore:

- `tine-settings.json`: `tine_core::model::atomic_update` under
  `SETTINGS_LOCK`.
- Workspace registry, including its session-to-workspace migration:
  `atomic_write_workspaces` -> `tine_core::model::atomic_write` under
  `WORKSPACES_LOCK`.
- Scoped session saves: `save_session_at` ->
  `tine_core::model::atomic_write` under `SESSION_LOCK`.
- Legacy session move: `migrate_legacy_session_at` -> rename plus
  `sync_dir_for_rename` under `SESSION_LOCK`.

No other production durable-state writer remains in `settings.rs`.

## Owned files and artifacts

- `src-tauri/src/settings.rs`
- `tests/regressions/non-ui.json`
- `CHANGELOG.md`
- `baseline-b5s-cargo-names.txt` (required pre-edit baseline; empty)
- `post-b5s-cargo-names.txt` (post-change comparison set; empty)
- `RECEIPT.md`

`tests/regressions/catalog.json` was inspected and validated but did not need a
delta: its existing `non-ui` inventory already indexes
`tests/regressions/non-ui.json`.

All forbidden B3-lane files and all new-module paths are untouched. No graph
or private corpus was read or used.

## Fail-before / necessity evidence

Before production changes, the new source-shape guard was run with:

```text
rtk bash -lc 'source scripts/env.sh; rtk proxy cargo test -p tine settings::tests::app_private_durable_publications_stay_on_named_audited_paths -- --exact --nocapture'
```

It failed on exact base shape with exit 101 and identified:

```text
605: std::fs::write(&tmp, data.as_bytes()).map_err(|e| e.to_string())?;
```

The failure stated that I-1/I-2 require every app-private durable publication
to use a named audited path and named the blessed
`atomic_write_workspaces` / `tine_core::model::atomic_write` exemplar. This
source-shape test is the honest necessity gate because a unit test cannot
observe whether bytes and the rename survived power loss.

## Pass-after evidence

- `rtk bash -lc 'source scripts/env.sh; rtk cargo test -p tine settings::tests::'`
  — 13 passed.
- `rtk bash -lc 'source scripts/env.sh; for iteration in $(rtk seq 1 20); do
  rtk cargo test -p tine
  settings::tests::concurrent_session_save_burst_keeps_a_complete_last_writer_and_no_temps
  -- --exact >/dev/null || exit 1; done'` — 20 consecutive focused runs passed.
- The following exact post-change command exited 0 and produced an empty full
  `cargo test -p tine` failure-name set (the baseline used the same selector):

  ```text
  rtk bash -lc 'set -o pipefail; source scripts/env.sh; rtk proxy cargo test -p tine 2>&1 | rtk awk '\''/^test .* \.\.\. FAILED$/ {name=$0; sub(/^test /, "", name); sub(/ \.\.\. FAILED$/, "", name); print name}'\'' | rtk sort -u | rtk tee post-b5s-cargo-names.txt'
  ```
- `rtk bash -lc 'rtk wc -l baseline-b5s-cargo-names.txt
  post-b5s-cargo-names.txt && rtk diff -u baseline-b5s-cargo-names.txt
  post-b5s-cargo-names.txt'` — no names added or removed (both zero lines).
- `rtk node scripts/check-regression-catalog.mjs` — regression catalog and
  two-inventory index OK.
- `rtk git diff --check` — clean.
- `rtk bash -lc 'source scripts/env.sh; rtk cargo fmt --all'` — run exactly
  once from the repository root; only the owned Rust file changed.

The behavioral tests prove that an invalid stale temp beside a valid session
is ignored, a concurrent burst always leaves one complete parseable writer
payload, successful writers leave no temps, readers never observe truncated
JSON, and the legacy migration moves complete bytes to the scoped path.

## Argued items and exclusions

- A crash-orphaned core temp is deliberately ignored rather than reclaimed by
  `load_session_at`. Its hidden name includes process id and a monotonic
  sequence and is opened with `create_new`; loaders read only the exact session
  name, so the orphan cannot become session authority. `create_new` also means
  a rare reused-pid/name collision is a reported save failure, never a clobber
  or false success. Deleting all matching temps would risk unlinking an active
  temp owned by a second process. Clean success and ordinary failure paths do
  reclaim their own temps.
- No `docs/storage-sync-contract.md` delta: this changes only the internal
  durability protocol of app-private device session state. It changes no graph
  layout, public surface, state machine, or schema.
- `rtk npm run check:regressions` validated the catalog, then exited 1 in the
  separate retired-managed-v1 source guard because unchanged
  `crates/tine-core/src/sync_runtime_tests.rs:27760` is classified as compiled
  legacy-v1 code. That file is outside the B5s write set and absent from this
  lane's diff; the direct catalog validator is green.
