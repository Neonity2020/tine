# Trusted-local foreground size cut

## Scope and baseline

This work started from `eac5a13f7e918cd084399a6b5e9f8c3d8cfbed76` on
`perf/trusted-local-size-cut`. All fixtures are synthetic Markdown and Org
graphs; no private graph was accessed.

The production coordinator probe reported one first commit at 2.64 ms with 100
unrelated pages and 126.06 ms with 10,000. The benchmark timed only
`TrustedLocalCommitCoordinator::commit`, but unlike the constituent managed
record benchmark it had no equal warm-up commits. The corrected probe keeps
page loading and finalized-operation preparation outside the interval, performs
two equal warm-ups for each fixture, and reports the p50 of five subsequent
commits.

## Measured cause

Test-only timers split commit into prepared-record construction, guarded graph
validation/append/publication/cache publication, hot-overlay apply, and direct
response materialization. The first diagnostic debug samples were:

| Unrelated pages | Prepared record | Guarded graph | Hot-overlay apply |
| ---: | ---: | ---: | ---: |
| 100 | ~30 ms | ~2.5 ms | ~29 ms |
| 1,000 | ~239 ms | ~2.5 ms | ~237 ms |
| 10,000 | ~2,335 ms | ~2.5 ms | ~2,341 ms |

Splitting managed candidate validation further put essentially the whole slope
in the per-update loop. For 1,000 pages, candidate validation took 473 ms, of
which 469 ms was the update loop: `clone_current_hot_document` took ~233 ms and
`current_hot_document_dependencies` took ~233 ms. Each independently called
`document_state::load_external_current` for the same touched scratch shard.
Preparation and overlay application both repeated those two accepted-checkpoint
loads, so the foreground commit loaded the same graph-sized authenticated
checkpoint four times. No derivative or pre-existing graph-wide counter saw
this work.

The graph publication review also found two avoidable generic cache paths:
rebuilding the complete effective-identity index after an exact content-only
publication, and materializing all real page names to scope derivative-cache
invalidation. They were not the dominant slope in this synthetic fixture (the
graph side contains one physical page), but both were proportional to graph
cache size and therefore inappropriate on the trusted-local foreground path.

## Removed work

The local authoring boundary already held the exact prospective post-documents.
Finalization now retains that point-addressed evidence for scratch-backed local
mutations and binds it to the finalized manifest fingerprint, engine generation,
and author-state root. Managed-record preparation and post-append application
reuse it only when all bindings, workspace/lineage, journal sequence, update
document set, managed-local eligibility, block claims, and deterministic
projection validation agree. Any miss uses the unchanged full validation and
replay path. Scratch archive staging deliberately ignores this foreground
evidence, so derivative publication behavior is unchanged.

For guarded graph publication, the retained complete graph-text admission index
already proves exact path and unique semantic ownership under the identity
mutation authority. The redundant legacy page-cache identity rebuild was
removed from verification. Exact content-only cache publication now invalidates
the legacy identity and derivative result caches in O(1), leaving their normal
on-demand rebuilds to readers instead of materializing graph-wide views in the
foreground.

Permanent structural counters cover accepted base-document loads, retained
author candidates, complete effective-identity rebuilds, and complete real-page
name materializations. The 12-edit semantic test and release probe assert zero
accepted base loads, two retained bounded candidates per edit (prepare and
apply), and a default/zero graph-wide work delta.

## Release result

One release build/probe was run with 7 commits per fixture, 2 warm-ups, and 5
measured samples:

| Stage p50 | 100 pages | 10,000 pages |
| --- | ---: | ---: |
| Total commit | 1.079795 ms | 1.382706 ms |
| Prepared record | 0.327177 ms | 0.477036 ms |
| Guarded graph total | 0.514365 ms | 0.616915 ms |
| Graph validation | 0.090838 ms | 0.110656 ms |
| Journal append | 0.016420 ms | 0.027351 ms |
| Graph publication | 0.405904 ms | 0.471386 ms |
| Graph cache publication | 0.028913 ms | 0.042219 ms |
| Hot-overlay apply | 0.226321 ms | 0.281081 ms |
| Direct response | 0.000110 ms | 0.000110 ms |

The 10,000-page p50 was 1.28x the 100-page p50 and both were below the 10 ms
gate. Both fixtures reported identical bounded work: 7 managed commits, 7
documents imported, 1,083 update bytes, 0 accepted base-document loads, 14
retained author candidates, and zero for every graph-wide structural counter.

## Correctness and recovery

The focused debug coordinator batch passed all seven non-ignored tests. It
covers Markdown and Org, nested/nonstandard paths, twelve consecutive edits,
preappend zero-effect failure, postappend recovery without reappend, and replay
on a fresh engine. Exact base bytes/revision, current file identity, parent
identity, path and semantic-owner collision checks remain before append. Journal
append durability and graph publication durability are unchanged. Overlay state
still advances only after the exact append receipt proves the one durability
barrier; when retained author evidence is absent after restart, the generic
decode/validate/apply path reconstructs the overlay without reappending.

Commands used for proof:

```text
RUST_MIN_STACK=134217728 cargo test -p tine-core --lib trusted_local_commit -- --test-threads=1
RUST_MIN_STACK=134217728 cargo test --release -p tine-core --lib trusted_local_commit_manual_release_boundedness_probe -- --ignored --nocapture --test-threads=1
```

## Changed files

- `crates/tine-core/src/oplog/hot_engine.rs`
- `crates/tine-core/src/oplog/trusted_local_commit.rs`
- `crates/tine-core/src/model.rs`
- `crates/tine-core/src/fast_commit.rs`
- `.replay-notes/trusted-local-size-cut.md`
