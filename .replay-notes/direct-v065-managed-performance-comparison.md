# Direct Files v0.6.5 versus the managed-storage candidate

Date: 2026-08-03

Revisions:

- exact Direct Files baseline binary/source: `v0.6.5`, `7f2a453a`
- managed candidate: `sync`, `9882f4c7`

## Conclusion

The current managed backend does not yet provide an across-the-board performance
improvement over v0.6.5 Direct Files.

- Managed startup makes the complete page catalog available faster at 1,000
  pages, but reaches the first requested page about 60 ms later at the core
  boundary.
- The current application in Direct Files mode reaches visible content 17.4%
  later than the exact v0.6.5 binary on the native startup fixture.
- A warm managed one-page save is about 200 times slower than a v0.6.5 Direct
  Files save on ext4 at 1,000 pages. The ratio measures extra oplog, archive and
  SQLite work, not the same operation regressing, but the user-visible latency is
  real.
- A deferred catch-up performed during managed clean shutdown took 208.7 seconds
  at 1,000 pages. That is a separate serious regular-use defect.

## Native application startup

Five alternating runs per binary used the existing `scripts/bench-startup.mjs`
harness, fresh XDG profiles, private D-Bus sessions and the same deterministic
80-page plus 120-journal-block graph.

| milestone | exact v0.6.5 | candidate, Direct Files | delta |
| --- | ---: | ---: | ---: |
| WebDriver session median | 216.213 ms | 216.057 ms | -0.1% |
| first visible content median | 576.174 ms | 676.475 ms | +17.4% |
| first visible content p95 | 612.977 ms | 685.260 ms | +11.8% |

Artifacts: `test-results/startup-v065-v-sync/summary.json`.

This native A/B deliberately kept both binaries in Direct Files mode. It proves
that process/session startup is unchanged and that current frontend/application
content readiness has regressed; it does not by itself measure a managed profile.

## Backend startup at 1,000 pages

The release-only `managed_startup_reopen_1000_page_manual_benchmark` activated
one 1,000-file / 9,972-block graph, then measured semantically equal Direct Files
and managed reads. Activation is outside these readiness milestones.

| milestone | Direct Files | managed |
| --- | ---: | ---: |
| backend open | 0.210 ms | 124.324 ms |
| first named-page request | 65.307 ms | 1.614 ms |
| total to first named page | 65.517 ms | 125.938 ms |
| whole page catalog after first page | 233.920 ms | 4.274 ms |
| total to whole page catalog | 299.437 ms | 128.598 ms |

Managed therefore reaches one requested page 60.4 ms later, but reaches complete
catalog readiness 170.8 ms sooner (2.33 times faster). The managed open adopted
the exact current state with zero replayed generations.

The same receipt exposed an independent cost:

| deferred operation | managed |
| --- | ---: |
| catch-up plus clean shutdown | 208,718.490 ms |

The first five-run invocation completed in 1,262.66 seconds because RTK hid the
per-phase output: one activation plus five copies of this shutdown cost. A
one-run raw receipt corrected the initial mistaken suspicion that reopen itself
was taking minutes.

## Warm one-page edits

The Direct Files baseline was measured from the exact v0.6.5 source using the
public audited `Graph::save_page` path. Each graph had 1,000 Markdown pages and
10 blocks per page. One existing page was changed ten times after two warm-ups;
all runs proved that exactly one file and one semantic page changed and that the
last edit survived a fresh reopen.

| complete backend save | p50 | p95/max |
| --- | ---: | ---: |
| v0.6.5 Direct Files, volatile overlay | 0.300-0.305 ms | 0.330-0.479 ms |
| v0.6.5 Direct Files, ext4/NVMe | 1.060-1.277 ms | 1.429-1.990 ms |
| managed local storage, volatile overlay | 212.515 ms | 377.535 ms |

At 100 pages, Direct Files was about 1.0-1.2 ms on ext4 while managed storage was
58.926 ms on the volatile overlay. Direct Files is essentially flat with graph
size; managed save latency still grows materially.

The same-filesystem overlay comparison is about 700 times apart. The more useful
real-disk comparison is approximately 170-200 times, though the managed ext4
number has not yet been measured. Both paths were timed around the complete
public backend call the editor awaits.

These operations are not semantically identical: Direct Files durably replaces
one Markdown file, while managed storage also drafts a CRDT transaction, stages
archive/index material, applies SQLite, and projects Markdown. The comparison
prices that architecture. It still establishes that ordinary managed edits are
not yet performance-competitive with v0.6.5.

The managed receipt used `/tmp`, whose volatile overlay makes `fsync` nearly
free. The old claim that managed warm saves floor around 45 ms because of fsync
is therefore unsupported by that receipt; most of the fixed floor is CPU and
memory work.

## Commands

```bash
TINE_MANAGED_STARTUP_BENCH_RUNS=1 TINE_ACTIVATION_TRACE=1 \
RUST_MIN_STACK=134217728 cargo test --release -p tine-core --lib \
  managed_startup_reopen_1000_page_manual_benchmark -- --ignored --nocapture

TINE_STARTUP_BASELINE=~/research/tine \
TINE_STARTUP_CANDIDATE=~/research/tine-sync \
TINE_STARTUP_RUNS=5 \
TINE_STARTUP_ARTIFACT_DIR=test-results/startup-v065-v-sync \
npm run bench:startup
```

The temporary exact-v0.6.5 edit harness was removed after three overlay and
three ext4 runs at each of 100 and 1,000 pages. Its full samples and source were
retained in the manager-side benchmark report while this note was prepared.

## Next performance work

1. Attribute and remove the 208-second deferred catch-up/shutdown path before
   attempting a 10,000-page startup run.
2. Treat a warm one-page save as a local-delta operation. The remaining 212 ms
   cannot be accepted as inherent CRDT or SQLite cost.
3. Re-run the managed save receipt on ext4, then measure a native activated
   profile against the exact v0.6.5 binary.
