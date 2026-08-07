# CI and release evidence

Tine concentrates broad CI at the release boundary. Ordinary coding should use
the causal test for the changed behavior plus directly affected neighbors; it
should not repeatedly start every platform build and performance comparison.
The frozen release candidate receives the exhaustive pass.

## What runs when

| Event | Automatic work | Purpose |
| --- | --- | --- |
| Non-doc pull request | `ci` → `PR validation / Linux unit and contract checks` | TypeScript, frontend and Rust-core tests plus cheap generated-artifact/release contract guards. No Windows, Android, performance, Flatpak build, or release packaging. |
| Docs/image-only pull request | No app CI | Avoid runner work for prose and image-only changes. A Flatpak/website metadata PR still gets its path-specific lightweight validator. |
| Push to `master` | No app test/build workflow | Merging does not repeat CI after the reviewed commit. Website pushes may still deploy Pages; issue automation is separate from app CI. |
| Manual `ci`, scope `full` | Linux contracts/tests plus four deterministic process-isolated `tine-core` nextest shards and the complete unpartitioned `tine-storage` suite, Windows compile-all `tine-core` and `tine-storage` test targets + contract-selected isolated core/storage smoke, Android core compile, same-runner performance A/B | Required exact-SHA release-candidate evidence. |
| Manual `ci`, focused scope | Only `windows`, `android`, or `performance` | Platform/performance proof while developing relevant changes. A focused run never satisfies the release gate. |
| Manual `ui-e2e` | Complete or scenario-focused Linux/Windows real-app proof | UI/harness debugging between releases without starting ordinary full CI. |
| Manual `Flatpak build test` | Real offline Flatpak build | Focused packaging proof. The release workflow calls the same workflow as a hard gate. |
| Manual/tagged `release` | Exact-SHA CI evidence check, release preflight, real Flatpak, desktop/Android packages, release E2E, assembly/publish | Expensive release proof. It fails before packaging if the exact candidate lacks successful full CI evidence. |

The lightweight pull-request path is a useful early signal, not release
evidence. Platform-native or observation-boundary proof remains necessary when
the changed behavior requires it.

## Frozen-candidate sequence

1. Finish release metadata and all source changes, freeze one commit, and push
   its branch.
2. Dispatch `ci.yml` on that branch with `scope=full` and wait for all nine full
   jobs, including the Linux inventory contract and each of its four hash shards.
   Record the exact commit and Actions run URL.
3. Optionally verify the same evidence from the checkout:

   ```bash
   GH_TOKEN="$(gh auth token)" node scripts/check-ci-evidence.mjs \
     --repo martinkoutecky/tine --sha "$(git rev-parse HEAD)"
   ```

4. Dispatch `release.yml` on the same frozen branch. Its preflight performs the
   evidence check independently before toolchain/dependency setup or packaging.
5. After the manual release matrix and candidate assembly succeed, tag that
   exact commit only with explicit release authority. The tag-triggered release
   repeats the evidence check before it builds or publishes artifacts.

Any source/rebase/version change creates a new SHA and invalidates both the full
CI evidence and the assembled candidate. Dispatch full CI again for the new SHA;
never reuse a green run from its parent. After a failure, rerun all jobs (not
only the failed job) so the latest workflow attempt contains fresh successful
evidence for all nine full lanes while Actions retains the failed attempt.

`scripts/check-ci-evidence.mjs` requires a completed manual `ci.yml` run whose
`head_sha` is exact and whose nine stable full-job names all concluded
`success`. That list includes all four Linux nextest shards, so a missing or
skipped shard cannot satisfy the gate. PR runs, focused runs, skipped jobs,
failed jobs, and merely green release runs cannot satisfy it.
`scripts/test-release-pipeline.mjs` keeps this fail-closed contract under
deterministic fixtures.

## Windows release scope

Linux is the complete behavior matrix for both crates: its nextest inventory
contract proves every non-ignored `tine-core` test runs exactly once across four
isolated shards, and every non-ignored `tine-storage` test runs in one
unpartitioned job. The storage half is newer than the rest of this document —
until 2026-08-07 no job on any platform executed the storage suite, because
Linux ran only `--package tine-core` while Windows compiled the storage targets
with `--no-run`. The contract now also asserts that the tests Windows defers are
executed on Linux, so a deferral cannot quietly mean "covered nowhere".

Windows is a deliberately narrower, blocking compatibility
gate: it compiles every `tine-core` and `tine-storage` test target and runs
declared core and storage smoke selections under nextest isolation. The core
selection contains every explicitly Windows-named core test plus the
bootstrap-capture, bootstrap-preparation, durability, and lifecycle witnesses
that caught the v0.6.90 Windows failures. The storage selection covers narrow
locking, directory-durability, journal durability/restart, and duplicate-open
contracts.

`scripts/tine-core-nextest-contract.mjs --mode windows --run-smoke` lists the
actual Windows inventories before executing the smoke. It fails if an explicitly
Windows-named core or storage test, or a declared witness, is added, renamed,
removed, or omitted from the exact selection/classification; it then runs the
verified core and storage sets with nextest's zero-retry, fail-on-timeout
profile. The sole currently deferred Windows-named storage test is explicitly
recorded in the contract rather than suppressed. This is neither an advisory
subset nor a retry mask.

Full runtime parity for all `tine-core` and `tine-storage` tests on Windows is
explicitly deferred. Some platform-neutral test fixtures currently encode
Unix-like file/identity assumptions; they remain fully blocking on Linux and do
not become a Windows Tine release requirement merely by being present in either
crate. Once `tine-storage` is independently versioned and pinned, its complete
cross-platform suite belongs to storage-version certification; ordinary Tine
releases should compile and integration-smoke the pinned version, while a storage
version or pin change forces recertification. Until then the crate remains an
in-tree path dependency, so Tine CI cannot honestly reuse certification from an
earlier storage version. Promoting a broader Windows matrix is a separate
compatibility project, not a quiet gate expansion during a release.

## Between releases

- Run focused local tests while editing and the affected behavior family's
  real-app proof before integration when relevant.
- Dispatch `ui-e2e` for Linux/Windows harness or native UI changes.
- Dispatch manual `ci` with `scope=windows`, `scope=android`, or
  `scope=performance` when that platform boundary is the thing being changed.
- Dispatch the Flatpak workflow for offline packaging changes.
- Do not dispatch `scope=full` as a routine completion ritual. It is the frozen
  release gate.
