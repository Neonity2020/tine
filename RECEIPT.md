# Harvest B3 implementation receipt

## Contract and base

Harvest packet B3 replaces WebView `localStorage` live-conflict retention with a named, graph-keyed, app-private native capsule protocol; adds Managed Storage capture without persisting replacement authority; gates graph activation on restoration; and routes both backends through one semantic review/resolution surface whose durable retirement precedes UI acknowledgement.

- Paused pre-fix head: `d9e100ce`.
- Master merged before implementation: `a3af1c1471f9635c5e6e0a13f741305876b8abce`.
- Post-merge implementation base: `1aaaa4da294e193b7f70de3af3240d3a344f2521`.
- Release/master integration was not attempted.

## Owned files

- Capsule protocol and registration: `src-tauri/src/conflict_capsule.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/managed_command_surface.rs`.
- Mode-aware capture and mode-blind review/resolution: `src-tauri/src/commands.rs`, `src/backend.ts`, `src/components/ConflictResolution.tsx`.
- Capture, restoration and awaited retirement: `src/persistence.ts`, `src/ui.ts`, `src/graph.ts`.
- Proof and contract surfaces: `scripts/e2e-concord-live-save.mjs`, `tests/ui-regressions/e2e-contracts.json`, `tests/regressions/non-ui.json`, `docs/storage-sync-contract.md`, `CHANGELOG.md`.
- Evidence: `baseline-b3-*.txt`, `b3-conflict-capsule-fail-before.log`, this receipt.

`src-tauri/src/commands.rs` is within the dossier's conditional write set: `capture_live_save_conflict` now crosses both storage modes, returning Direct's exact observed payload and deliberately returning no persistable replacement authority for Managed.

## Exclusions honored

- `src-tauri/src/settings.rs` was not edited.
- No release state, master checkout, queue card, deployment, or integration operation was changed.
- No migration machinery was added; first use only deletes `tine.concord.live-conflicts.v1`.
- The forbidden brain corpus was not read, searched, indexed, or used.

## Fail-before evidence

- The regression-catalog row was committed before the fix at `d9e100ce`.
- `b3-conflict-capsule-fail-before.log`: the native protocol necessity selection failed pre-fix with 1 passed / 5 failed because the app-private capsule channel did not exist.
- Source-level necessity accepted from the dossier: the previous Direct path wrote only localStorage and Managed had no durable capsule.

## Implementation evidence

- Fixed layout: app-data `conflict-capsules/<session-style-graph-key>.v1.json`, envelope `{version:1,capsules:[...]}`.
- Publication and rewrite use `tine_core::model::atomic_write`; stale torn temps are ignored and reclaimed; the last retirement removes the envelope and synchronizes its directory.
- Native tests cover exact bytes, torn-temp recovery, replacement cut/reopen, retirement/reopen, final removal, and graph-key separation: 6 passed.
- Managed capsules persist retained page/base binding only. Replacement path/revision is re-observed after restart and kept in live UI state only.
- `restoreLiveSaveConflicts(root)` is awaited before the graph becomes interactive. Resolution awaits native capsule retirement before clearing the banner or acknowledging success.

## Green gates

- Fresh pre-edit baselines after the master merge: npm fully collected and green; `npx tsc --noEmit` green; cargo 342 passed / 0 failed / 2 ignored.
- `npx tsc --noEmit`: green.
- `npm test -- --run`: green; core 3278 passed, render 1463 passed, deploy-profile contract OK; zero failure names in either direction against the baseline.
- `cargo test -p tine`: green; 350 passed / 0 failed / 2 ignored; zero failure names in either direction against the baseline.
- Focused native protocol tests: 6 passed.
- Focused Managed mode-blind adapter tests: 2 passed.
- `cargo fmt --all -- --check`: green after the dossier's single final formatting pass.
- `npm run build`, debug Rust build, and release Rust build: green.

## Real-app E2E quarantine

The required real-app matrix is not green, so B3 is not complete and must not integrate.

1. The first debug and bare-release launches never reached Tine: both loaded `about:blank` with `Could not connect to localhost: Connection refused`. Repository documentation in `src-tauri/Cargo.toml` identifies the cause: bare cargo release builds omit Tauri's `custom-protocol` feature.
2. A valid embedded binary was then built with `cargo build -p tine --release --features custom-protocol`. It reached Tine, but the Direct setup stopped before conflict capture: `Keep Draft was absent from quick switch`.

The dossier's two-attempt stop-loss is exhausted. No further journey tuning was performed. The unproved outcomes are kill/restart restoration on both backends, both post-restart resolution actions through the one component surface, Managed stale-owner re-observation in the real app, and on-disk retirement before acknowledgement.

## Forks and argued cuts

- Fork: async restoration is resolved by the dossier's required awaited graph-load gate; no sync shim or eventual restore.
- Fork: on-disk layout is the dossier-fixed single graph-keyed v1 envelope; no per-page or generation design.
- Fork: component surface is mode-blind; native/backend dispatch owns storage-specific authority.
- Delegated design: Managed transient authority is a tagged live review result and is never serialized in the capsule.
- Argued cut: no localStorage migration beyond deletion. Losing an upgrade-mid-conflict capsule on profiles where the old store happened to persist is accepted by D-1 and the dossier.
- Argued cut: browser/test fallback retains an in-memory async-equivalent cache; native graph activation always awaits the app-private file.

## Verdict

**QUARANTINED CHECKPOINT.** Native protocol, crash cuts, typecheck, full UI/render suites, full Rust suite, formatting and builds are green. The mandatory real-app journey stopped at setup and is quarantined under the packet stop-loss. Preserve and push the topic branch; do not stage, integrate, deploy, retire the worktree, or mark the catalog row fixed.
