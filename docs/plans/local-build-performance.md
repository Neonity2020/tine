# Local topic-batch build performance

Measured on the persistent Linux development host on 2026-08-24 at
`16812f929d344862ddc95a3414b02a2ed6f240f3`.

## Baseline

`cargo test -p tine-core --no-run` in a new worktree with an empty local
`target/` took 78.13 seconds wall time, 338.09 CPU-seconds, 4.98 GB peak RSS,
and wrote about 25.1 GB. An immediately repeated no-op command took 0.50
seconds. Touching `crates/tine-core/src/lib.rs` without changing its contents
then took 13.74 seconds. This establishes that Cargo's within-worktree
fingerprints/incremental state work well; the repeated cost comes primarily
from every worktree starting with an isolated empty target directory.

The primary checkout's `target/` occupied 63 GB and the historical permanent
OpenCode checkout occupied 18 GB. The machine had 264 GB free before this
measurement. Tine's `tine-core` remains a large crate, but these measurements
do not by themselves justify a risky storage-adjacent crate split.

## Rejected shared compiler cache

The checksum-verified upstream `sccache` v0.17.0 binary was tested with Rust
incremental output disabled. A cold seed took 71.23 seconds. Repeating from an
empty target at the same canonical in-sandbox source/output path produced Rust
cache hits, but failed immediately because a cached build-script invocation
referenced `libautocfg-03d23736f0cab9a1.rlib` and that dependency had not been
restored. A retry failed identically. Earlier tests with distinct output paths
had zero Rust hits and took 71.39 seconds. This is not a safe build foundation;
the binary/cache were removed and Tine does not enable `RUSTC_WRAPPER`.

## Change

- When the coordinator sets `TINE_FAST_LOCAL_BUILD=1`, `scripts/deploy.sh`
  retains the optimized release profile and `target/release/tine` contract but
  uses 16 codegen units and incremental compilation for a faster local test
  binary.
  Public release candidates leave this unset and continue using the repository's
  deterministic `codegen-units=1` profile.
- Focused checks run while issues are implemented; complete typecheck/unit/native
  gates and deployment run once per coherent multi-issue topic batch. Exact-head
  gates are rerun only after the final merge from current master or a source
  correction invalidates them.

The local release-profile comparison was run after the frontend had been built.
Changing profiles necessarily invalidates existing release artifacts, so the
profile-switch numbers are not normal no-op builds; they measure the one-time
cost when a worktree first adopts a profile. Switching to the fast local profile
took 138.68 seconds, while switching back to the deterministic profile took
213.69 seconds. More importantly for normal issue work, touching
`crates/tine-core/src/lib.rs` without changing its contents and rebuilding took
8.72 seconds under the warm fast profile versus 210.73 seconds under the warm
deterministic profile. A fast-profile no-op check took 4.29 seconds. These are
single-host observations rather than a portable benchmark, but the edit-build
difference is large enough to justify the explicit local opt-in.

## Decision boundary

Keep worktree-local `target/` directories: a single shared Cargo target would
serialize otherwise-disjoint parallel batches on Cargo locks and mix branch
fingerprints. Re-measure real overnight batches after the workflow change.
Consider splitting `tine-core` only if profiles still show a persistent large
edit-build cost in one code area; never split Direct Files or managed-storage
authority merely to reduce compile time.
