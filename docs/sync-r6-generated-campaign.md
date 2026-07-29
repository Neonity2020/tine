# R6 generated simulator campaign

`oplog_generated_campaign` is a deterministic, production-simulator campaign.
It serializes each scenario through the existing canonical `Scenario` format;
the simulator runs its global store and convergence oracles after every action.
It does not provide a second merge model.

Five families are selected directly from the seed. `seed % 5` chooses the
family and `(seed / 5) % 4` chooses its four-way variant. Thus every seed has
one stable family and canonical encoded scenario; no random generator state or
replayed expected state participates in generation.

- `r6-generated-transport-v1` exercises manifest-first and object-first
  delivery, reverse object ordering, duplicate provider visibility,
  partition/heal, restart, idempotent rescan, and replica convergence.
- `r6-generated-independent-page-edits-v1` gives two devices a common root,
  then makes independent edits to different pages and delivers one edit twice.
- `r6-generated-same-target-conflicts-v1` varies same-block text/text,
  edit/delete, move/delete, and rename plus referrer edit. Two observer
  replicas receive the concurrent batches in opposite orders, and every case
  includes a duplicate delivery. Its only semantic oracle is the simulator's
  production convergence invariant.
- `r6-generated-coordinator-external-v1` uses `CoordinatorAction` against
  nested Unicode Markdown and Org paths, with all LF/CRLF combinations. Its
  four variants collectively cover real external write, rename, deletion, and
  a projection interruption followed by crash/reopen/retry. Their expectations
  are coordinator evidence and explicit checkpoints, including the meaningful
  empty managed-file image after deletion.
- `r6-generated-coordinator-sqlite-rebuild-v1` varies SQLite delete,
  truncation, corruption, and stale frontiers, paired with coordinator
  interruption points during/after SQLite application and before/during
  projection. It explicitly proves the read gate closes after damage, then
  uses production accepted-archive and materialization checkpoints to prove
  the rebuild equals saved derived state.

The normal fixed corpus contains 20 consecutive seeds, so each family appears
four times and all four variants run at least once. Fast tests use a typed
deterministic fixture candidate. The ignored burn-in instead requires
`TINE_R6_FROZEN_CANDIDATE` to name the exact full Git object ID or SHA-256
frozen-patch digest under test. It bounds the count to 100,000 and rejects a
seed whose requested range would wrap `u64`; the test output records that
caller-supplied candidate. On failure, the harness writes the canonical
original scenario, metadata, and (when the existing exact-identity minimizer
can preserve the failure) its minimized scenario and failure capsule. Artifacts
default to `target/tine-r6-generated-campaign-artifacts`; set
`TINE_R6_CAMPAIGN_ARTIFACT_DIR` to select an explicit retained directory.

Normal focused commands:

```bash
rtk cargo test -p tine-core --test oplog_generated_campaign generated_campaign_fast_default
rtk cargo test -p tine-core --test oplog_generated_campaign generated_campaign_is_byte_identical_per_seed
rtk cargo test -p tine-core --test oplog_generated_campaign generated_campaign_known_seed_range_replays_green
```

After the manager commits the exact branch commit, run the formatter, focused
compile check, exact integration target, and diff check before the bounded
burn-in:

```bash
rtk cargo fmt --all
rtk cargo check -p tine-core --test oplog_generated_campaign
rtk cargo test -p tine-core --test oplog_generated_campaign
rtk git diff --check HEAD^ HEAD
```

The ignored burn-in requires an explicit decimal seed and bounded count. This
post-commit example persists any failure capsule beneath
`/tmp/tine-r6-campaign-artifacts` and records the exact manager commit as the
caller-supplied frozen candidate:

```bash
rtk env TINE_R6_FROZEN_CANDIDATE="$(rtk git rev-parse HEAD)" TINE_R6_CAMPAIGN_SEED=70600 TINE_R6_CAMPAIGN_COUNT=64 TINE_R6_CAMPAIGN_ARTIFACT_DIR=/tmp/tine-r6-campaign-artifacts cargo test -p tine-core --test oplog_generated_campaign generated_campaign_burn_in -- --ignored --nocapture
```

The test prints frozen candidate, seed, count, elapsed milliseconds, and
scenarios/second.
