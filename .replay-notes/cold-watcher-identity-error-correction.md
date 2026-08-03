# Cold watcher identity error correction

## Reproduction

Starting from `c2c786856e203b7ec036f55d0cafa9b880b0e3f2` on
`fix/cold-watcher-identity-error`:

```text
rtk env RUST_BACKTRACE=1 cargo test -p tine-core 'watcher_failures' -- --nocapture
```

Both named tests failed (`0 passed; 2 failed`). The public refusal expected
`io::ErrorKind::AlreadyExists`, but `save_page` returned `InvalidData`.
Re-running through `rtk proxy` after adding error display to the assertions
captured the producing path:

```text
guarded graph-text identity construction failed: graph text is not UTF-8
```

The failing save was name-only creation after `sync_file_checked` had already
recorded the watcher failure and installed an `EffectiveIdentityIndex` whose
generation matched `cache_generation()` and whose failure list named the
unreadable path.

## Root cause and classification

Classification: validation in the wrong order.

The effective identity evidence was correct and generation-bound. The retained
guarded collision-index integration changed `validate_graph_text_target` to
rebuild and semantically decode the guarded index before consulting that
effective evidence. When the watcher failure was invalid UTF-8 (or otherwise
undecodable), the rebuild returned `InvalidData` first. The intended name-only
fail-closed check therefore never returned its `AlreadyExists` conflict
contract. This was not a wrong-evidence publication and not a correct refusal
that merely needed remapping.

## Behavior before and after

Before: current watcher failure evidence blocked creation in principle, but a
guarded semantic rebuild reparsed the failed file first and leaked
`InvalidData`.

After: for a missing name-only target with recorded page-index failures,
`validate_graph_text_target` consults the existing generation-bound effective
identity evidence before attempting the guarded semantic rebuild. The save
returns `AlreadyExists` without writing. Successful watcher repair still
reconciles the file, clears the failure evidence, publishes the repaired owner
at the new generation, and permits later creation.

## Files changed

- `crates/tine-core/src/model.rs`: order the watcher-failure evidence check
  before guarded semantic reconstruction; make both focused assertions include
  the actual error text on failure.
- `.replay-notes/cold-watcher-identity-error-correction.md`: this proof record.

## Verification

```text
rtk cargo test -p tine-core 'model::tests::cold_watcher_failures_publish_generation_bound_identity_evidence_until_repair' -- --exact
# 1 passed

rtk cargo test -p tine-core 'model::tests::warm_install_and_watcher_failures_replace_effective_identity_evidence' -- --exact
# 1 passed

rtk cargo test -p tine-core 'model::tests::watcher_'
# 2 passed

rtk cargo test -p tine-core 'effective_identity'
# 4 passed

rtk cargo test -p tine-core 'guarded_identity'
# 3 passed

rtk cargo fmt --all
# passed

rtk cargo check -p tine --all-targets
# passed with 0 errors and 46 pre-existing warnings

rtk git diff --check
# passed
```
