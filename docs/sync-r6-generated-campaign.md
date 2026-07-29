# R6 generated simulator campaign

`oplog_generated_campaign` is a deterministic, production-simulator campaign.
It serializes each scenario through the existing canonical `Scenario` format;
the simulator runs its global store and convergence oracles after every action.
It does not provide a second merge model.

The first two families are intentionally small:

- `r6-generated-transport-v1` exercises manifest-first and object-first
  delivery, reverse object ordering, duplicate provider visibility,
  partition/heal, restart, idempotent rescan, and replica convergence.
- `r6-generated-independent-page-edits-v1` gives two devices a common root,
  then makes independent edits to different pages and delivers one edit twice.

Fast tests use a typed deterministic fixture candidate. The ignored burn-in
instead requires `TINE_R6_FROZEN_CANDIDATE` to name the exact full Git object ID
or SHA-256 frozen-patch digest under test. On failure, the harness writes the
canonical original scenario, metadata, and (when the existing exact-identity
minimizer can preserve the failure) its minimized scenario and failure capsule.
Artifacts default to `target/tine-r6-generated-campaign-artifacts`; set
`TINE_R6_CAMPAIGN_ARTIFACT_DIR` to select an explicit retained directory.

Normal focused commands:

```bash
rtk cargo test -p tine-core --test oplog_generated_campaign generated_campaign_fast_default
rtk cargo test -p tine-core --test oplog_generated_campaign generated_campaign_is_byte_identical_per_seed
rtk cargo test -p tine-core --test oplog_generated_campaign generated_campaign_known_seed_range_replays_green
```

The ignored burn-in requires an explicit decimal seed and bounded count. This
example persists any failure capsule beneath `/tmp/tine-r6-campaign-artifacts`:

```bash
rtk env TINE_R6_FROZEN_CANDIDATE="$(git rev-parse HEAD)" TINE_R6_CAMPAIGN_SEED=70600 TINE_R6_CAMPAIGN_COUNT=64 TINE_R6_CAMPAIGN_ARTIFACT_DIR=/tmp/tine-r6-campaign-artifacts cargo test -p tine-core --test oplog_generated_campaign generated_campaign_burn_in -- --ignored --nocapture
```

The test prints seed, count, elapsed milliseconds, and scenarios/second. Later
corpus rounds should add the remaining external-file, coordinator, SQLite, and
same-target conflict families rather than widening these starter schedules.
