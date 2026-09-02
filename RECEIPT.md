# Harvest B5p receipt — durable immutable plugin packages

## Contract and pin

- Packet: B5p, invariants I-1, I-2, I-16.
- Tine base: `e19514f9` (Wave 1 cumulative branch).
- External protocol release: `tine-storage` `v0.12.0`, commit
  `51688bae28a6d065bdd4668f70a746d179388826`.
- Certified tagged workflow: <https://github.com/martinkoutecky/tine-storage/actions/runs/33575662347>.
- Release: <https://github.com/martinkoutecky/tine-storage/releases/tag/v0.12.0>.
- Receipt SHA-256:
  `65e6571be92f001abae994da1a74bb427356b968e7752778571b3f498100f60e`.
- Attestation: <https://github.com/martinkoutecky/tine-storage/attestations/44567645>.

The storage release adds the minimal staged-directory package protocol:
per-file durability, stage-directory barrier, native no-clobber whole-directory
publication, both Unix parent barriers, Windows write-through name operations,
retire-then-reclaim, and deterministic recovery. Tine calls that protocol and
retains plugin-specific validation and settings ordering.

## Owned files and exclusions

Tine owned files changed:

- `src-tauri/src/plugins.rs`, `src-tauri/Cargo.toml`
- `crates/tine-core/Cargo.toml`, `crates/tine-core/src/projection_producer_census.rs`
- `Cargo.lock`, `flatpak/cargo-sources.json`
- `docs/storage-sync-contract.md`
- `docs/dependency-receipts/tine-storage.json`
- `docs/dependency-receipts/tine-storage-v0.12.0.txt`
- `tests/regressions/non-ui.json`, `CHANGELOG.md`
- this receipt and the two B5p evidence files

The separately released `tine-storage` commit owns `src/package_store.rs`, the
cross-parent no-clobber directory primitive in `src/filesystem.rs`, its public
surface, tests, contract, changelog, version, API golden, and certification
workflow evidence.

Explicit exclusions remained untouched: `src/plugins/manifest.ts`,
`PLUGIN_CAPABILITIES`, `src/plugins/capabilityBoundary.test.ts`,
`src-tauri/src/settings.rs`, and `publish.rs`. D-8 is preserved: this changes
package durability only and introduces no plugin capability.

## Durable-write census

| Durable class | Named audited protocol | Result |
| --- | --- | --- |
| Registry cache/settings | existing `update_settings_strict_at` / `update_settings` | unchanged, audited |
| Package install bytes | `tine_storage::publish_package_noclobber` | staged, durable, exact no-clobber |
| Uninstall settings | existing `update_settings` before byte retirement | unchanged ordering |
| Uninstall package bytes | `tine_storage::retire_package` | durable retire, then reclaim |
| Store-open recovery | `tine_storage::recover_package_store` | staged, retired, and incomplete active packages reclaimed |
| Raw production `write`/`rename`/`remove_dir*` in `plugins.rs` | none | class reduced to zero; guard-pinned |

## Proof and necessity

`necessity-b5p.txt` records both fail-before observations: the old production
region contained bare write/rename/delete operations, and a seeded
`plugin.wasm`-only half-package was neither installable nor uninstallable.

The storage layer exercises every modeled publish and retirement crash cut,
reopen/recovery, two-writer exact collision behavior, transient cleanup,
symlink containment, and the five-target source guard. The app layer proves
the settings-clear/before-retirement orchestration cut, recovery of transient
and wedged package shapes, concurrent different-byte installs, transient-name
grammar disjointness, and the production source shape.

Missing barriers are not honestly observable on the ordinary test filesystem;
behavioral crash-cut tests cover observable state outcomes and source-shape
guards pin the barriers and platform branches. No simulated fsync success is
presented as power-loss proof.

## Green gates

- `tine-storage` pre-tag and tagged certification matrices: Linux, Windows,
  Android compile, public API/semver, receipt and attestation all green.
- `tine-storage cargo test --locked`: 176 passed, 1 ignored.
- `tine-storage cargo test --locked --all-features -- --test-threads=1`:
  176 passed, 1 ignored.
- `cargo fmt --all` exactly once at the final Tine Rust edit; subsequent
  `cargo fmt --all -- --check` green.
- `cargo test -p tine --lib`: 358 passed, 2 ignored.
- Focused plugin suite: 17 passed.
- Projection producer census pin test: 1 passed.
- Baseline/final name comparison: 355 to 360, no removals, exactly the five
  B5p tests listed in `baseline-b5p.txt` added.
- `npm test`: green (logic, render, deploy-profile).
- `npx tsc --noEmit`: green.
- `npm run build`: green; storage and wasm pins, lsdoc oracle, and Vite build.
- `npm run bench`: completed; cross-machine numbers explicitly advisory.
- `node scripts/check-storage-pin.mjs`: green.
- `node scripts/check-regression-catalog.mjs`: green (385 UI entries, 252
  GitHub issues, two inventories).
- Flatpak cargo-source coverage: green (709 registry packages, 4 git packages).
- `git diff --check`: green.

`npm run check:regressions` additionally reaches the inherited
`check-retired-managed-v1` failure at
`crates/tine-core/src/sync_runtime_tests.rs:27934`; the same failure reproduces
unchanged on the exact packet base. It is not caused by, or inside, B5p's write
set. The required regression-catalog gate itself is green.

## Fork resolutions

- Placement fork: resolved to `tine-storage`, the owner of the existing
  durable-publication primitives; no protocol is duplicated in Tauri.
- Five-target fork: reused the already certified v0.11 no-clobber primitive and
  added a pinned Linux/macOS/iOS/Android/Windows guard for the new package move.
- Collision fork: exact same bytes are idempotent; different bytes are refused
  only after the no-clobber outcome, never after an app-layer exists check.
- Removal fork: settings clear stays first; bytes use retire-then-reclaim and
  store-open recovery.
- D-8 fork: no capability surface changed.
- Guide fork: exempt, because this is internal package durability with no new
  user workflow.
- Anonymized-graph fork: exempt; plugin package storage is app-private and does
  not depend on graph corpus content.

## Verdict

PASS. B5p replaces every production plugin-package write/removal path with one
certified audited protocol, preserves immutable-version and uninstall ordering
semantics, recovers every required residue shape, pins all five shipped targets,
and does not widen plugin authority.
