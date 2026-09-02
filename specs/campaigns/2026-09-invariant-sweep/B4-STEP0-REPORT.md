# B4 step 0: query coverage and measurement

Date: 2026-09-02. Commit: `e928b18400e0622950b53f54444b34050df0c493`.
This packet is measurement only. It changes no production query behavior, storage
layout, state machine, schema, or public surface, and it adds no bench-policy gate.

## Executive result

Page references are the recommended first new grammar class. Across the three
fixture graphs they occur in four of twelve queries and cost 8.951 ms per
invalidated query on the 1,047-page benchmark graph, for the largest measured
frequency-times-walk-cost score (35.804 ms). The schema, Direct lowering, and
`page_referrer_candidates_after` read already exist. Task is more frequent, but
it already has a Direct sparse-SQL path; its immediate post-edit readiness is a
separate problem.

The A4 observation is not a production save number. The repro explicitly drained
after every page. At 400 pages, that harness-created drain consumed 463,853 ms,
99.2% of observed elapsed time, versus 3,564 ms of cumulative foreground saves.
The last forced drain grew 8.06x from 50 to 400 pages, led by `tail_and_sqlite`
and its nested `projection_adoption` stage. Foreground save still grows about
8x and merits its own fix lane, but it is not the source of the archived
52-minute attribution as previously framed.

## Grammar-to-plan matrix

“Plan” is `simple_query_candidate_plan`; “sparse” is
`sparse_task_query_eligibility`; “Managed” is the existing plan-to-read arm.
`All` means a parser walk. Read names are the existing `tine-storage v0.12.0`
families, not proposed APIs. “Lowered” refers to the existing Direct projection.

| `Pred` production | Plan today | Direct sparse today | Managed narrowing today | Existing `SqliteGraphProjectionRead` family | Direct facts already lowered |
| --- | --- | --- | --- | --- | --- |
| `PageRef` | `PageRef` | no | yes | `page_referrer_candidates_after`; exact navigation-page reads for page-self membership | page names and block/page references: yes |
| `Task` | `Task` | yes, alone or task-anchored `And` | yes | `task_candidate_pages_after`, `tasks_after`, `task_blocks_after` | marker, priority, scheduled, deadline: yes |
| `Priority` | `All` alone | yes only inside task-anchored positive query; at most one | only through sparse task sibling, else `All` | task families above | yes |
| `Property` | `BlockProperty` | no | yes | `block_property_candidates_after`; `properties_named_after` / facet rows for values | block properties: yes |
| `Scheduled` | `All` alone | yes only task-anchored | only through sparse task sibling, else `All` | task families above | yes |
| `Deadline` | `All` alone | yes only task-anchored | only through sparse task sibling, else `All` | task families above | yes |
| `Journal` | `Journal` | no | inventory then filter | `navigation_pages_after` | page kind/date metadata: yes |
| `Between` | `Journal` only for journal field; otherwise `All` | scheduled/deadline only when task-anchored | journal inventory, sparse task for scheduled/deadline, otherwise `All` | navigation-page or task families | yes for those fields |
| `Page` | `Page` | no | inventory then filter | `navigation_pages_by_name_key_after` | normalized page names: yes |
| `Namespace` | `Namespace` | no | inventory then filter | `navigation_pages_by_namespace_after` | normalized page names: yes |
| `PageProperty` | `PageProperty` | no | yes, currently via all facet rows | `properties_named_after`, `property_facet_rows_after` | page properties: yes |
| `PageTags` | `PageProperty(tags)` | no | yes, currently via all facet rows | `tags_after`, `properties_named_after` | tags/page properties: yes |
| `Content` | `All` | no | `All` | `plain_text_candidate_pages_after` can safely narrow a nonempty literal | searchable page/block text: yes |
| `Search` | `All` | no | `All` | `search_after` is raw FTS; friendly-search semantics still need safe candidate extraction | searchable page/block text: yes |
| `ContentRegex` | `All` | no | `All` | none with Rust-regex semantics; SQLite cannot express this class through the current API | text exists, regex operator does not |
| `And` | first complete child plan | yes only when all children are admitted and task-anchored | yes, through first complete child | family of selected child | component-dependent, generally yes |
| `Or` | union only when every branch has a complete plan | no | yes when complete, otherwise `All` | union of child families | component-dependent, generally yes |
| `Not` | `All` | no | `All` | no safe negative-only candidate without a universe/anti-join read | child facts may exist; safe candidate operator does not |
| `Sample` | `All` alone | neutral only inside admitted task query | only through a planned sibling | no candidate read needed; finalizer | not applicable |
| `SortBy` | `All` alone | neutral only inside admitted task query | only through a planned sibling | no candidate read needed; finalizer | relevant task/page metadata exists |
| `Aggregate` | `All` alone | neutral only inside admitted task query | only through a planned sibling | no candidate read needed; finalizer | not applicable |
| `GroupBy` | `All` alone | neutral only inside admitted task query | only through a planned sibling | no candidate read needed; finalizer | not applicable |

The hard constraint is explicit: `ContentRegex` cannot be moved to the current
SQLite read surface while retaining Rust-regex semantics. `Not` also remains a
walk unless a positive sibling supplies a complete candidate set. Friendly
`Search` is not equivalent to raw FTS and must retain oracle comparison and a
fallback even though a read family exists.

## Count-only corpus census

The scanner recognizes `{{query …}}` and `#+BEGIN_QUERY` forms and reports only
counts. No corpus text was emitted, retained, or copied into this report.

| Graph | Files | Queries | Simple | Advanced | Invalid |
| --- | ---: | ---: | ---: | ---: | ---: |
| tine-test | 23 | 8 | 8 | 0 | 0 |
| kitchen-sink | 1 | 3 | 3 | 0 | 0 |
| org graph | 22 | 1 | 1 | 0 | 0 |
| anonymized graph | 1,045 | 0 | 0 | 0 | 0 |

| Production | tine-test | kitchen-sink | org | anonymized | total query containment |
| --- | ---: | ---: | ---: | ---: | ---: |
| `Task` | 5 | 2 | 1 | 0 | 8 |
| `PageRef` | 3 | 1 | 0 | 0 | 4 |
| `And` | 2 | 1 | 0 | 0 | 3 |
| `Priority` | 2 | 0 | 0 | 0 | 2 |
| `Page` | 0 | 1 | 0 | 0 | 1 |
| every other matrix row | 0 | 0 | 0 | 0 | 0 |

Containment counts overlap: an `And` query can also contain `Task` and
`Priority`. The twelve query count must therefore not be obtained by summing
the production rows.

## Direct query measurements

The env-selected anonymized graph was copied to scratch and augmented with two
synthetic target pages; the timed graph therefore had 1,047 pages and 14,536
blocks. Each reported row is the median of three release-benchmark invocations,
nine samples per invocation. “Memo” repeats an unchanged query. “Invalidated”
performs a content-only save, waits for projection readiness, then queries.

| Class | memo median/p95/max ms | invalidated-ready median/p95/max ms | share of summed invalidated medians | invalidated/memo | indexed reads/run |
| --- | ---: | ---: | ---: | ---: | ---: |
| sparse task | 0.004058 / 0.011421 / 0.011421 | 1.652907 / 2.504527 / 2.504527 | 9.6% | 407.32x | 9 |
| page reference | 0.001703 / 0.004479 / 0.004479 | 8.950936 / 15.326133 / 15.326133 | 51.8% | 5,255.98x | 0 |
| block property | 0.001412 / 0.002806 / 0.002806 | 1.481619 / 2.066163 / 2.066163 | 8.6% | 1,049.31x | 0 |
| page property | 0.001493 / 0.002775 / 0.002775 | 1.011716 / 1.190088 / 1.190088 | 5.9% | 677.64x | 0 |
| page tags | 0.002234 / 0.004097 / 0.004097 | 0.299376 / 0.340642 / 0.340642 | 1.7% | 134.01x | 0 |
| plain text | 0.000982 / 0.003046 / 0.003046 | 1.121781 / 1.473513 / 1.473513 | 6.5% | 1,142.34x | 0 |
| friendly search | 0.001703 / 0.003686 / 0.003686 | 1.371705 / 1.604667 / 1.604667 | 7.9% | 805.46x | 0 |
| boolean composition | 0.002264 / 0.005029 / 0.005029 | 1.393947 / 1.505984 / 1.505984 | 8.1% | 615.70x | 0 |

The required discriminator is present but not universal: sparse task is 5.42x
faster than the dominant page-reference walk, while tags and several small walk
classes are faster than sparse task on this graph. SQL row hydration and final
evaluation make the task route nonzero; the premise “SQL is below every walk”
does not hold at this size. The useful discrimination is indexed-read evidence
plus the page-reference ratio, not a blanket ordering.

Immediately after each of 27 content saves, projection readiness was 0/27 and
the task query used indexed reads 0/27; median fallback time was 0.492804 ms.
This is explicit harness timing at `dataRev` time: `enqueue_delta` marks the
projection not ready, and only later reference reads wait. It does not imply
that steady-state production has a 0% projection hit rate.

### Facet reads

The facet fixture used 24 blocks per page. These results measure the two
existing facet consumers separately from grammar evaluation.

| Read | 1,001 pages / 24,001 blocks | 4,001 pages / 96,001 blocks | growth for 4x pages |
| --- | ---: | ---: | ---: |
| `autocomplete_property_facets_bounded` | 0.473709 ms | 3.857517 ms | 8.14x |
| `query_facets` | 0.174214 ms | 2.166199 ms | 12.43x |

The current facet reads therefore grow faster than block count in this two-size
measurement; B4a cannot yet claim independence from graph size.

## Invalidation measurement

All edit classes increment `cache_gen`. Scoped content invalidation retags
survivors rather than dropping `DerivedCache`. Four seeded memo entries were
measured on the anonymized graph.

| Edit class | generation delta | retained | evicted | interpretation |
| --- | ---: | ---: | ---: | --- |
| content-only | +1 | 2/4 | 2/4 | affected content query evicted; unrelated task/page-ref retained; an `SQ` key is conservatively unknown and evicted |
| alias/page-set | +1 | 0/4 | 4/4 | page-set semantic change takes the non-scoped branch |
| day rollover (simulated) | +1 | 0/4 | 4/4 | date-sensitive global invalidation |

This falsifies “every save drops the memo.” Current content-only saves perform
scoped eviction plus generation retagging; wholesale drop is limited to the
non-scoped and day-rollover branches.

## A4 save-versus-drain attribution

The release-only ignored test was run three times at each cadence. “Never” means
no drain inside the measured loop; a final settle happens only after all timed
checkpoints so the legacy reopen assertion remains valid. “Every page” is the
archived repro’s explicit harness behavior, not ordinary production behavior.

| cadence | pages | last save ms | cumulative save ms | cumulative drain ms | drain share of elapsed | pending at checkpoint |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| never | 50 | 2.256218 | 84.005 | 0.000 | 0.0% | 50 |
| never | 200 | 7.841965 | 831.089 | 0.000 | 0.0% | 200 |
| never | 400 | 17.835903 | 3,363.761 | 0.000 | 0.0% | 400 |
| every page | 50 | 2.458242 | 87.503 | 1,293.502 | 93.8% | 0 |
| every page | 200 | 8.173528 | 911.721 | 52,749.779 | 98.3% | 0 |
| every page | 400 | 19.231601 | 3,563.768 | 463,853.478 | 99.2% | 0 |

Representative foreground save-stage medians from the no-drain branch:

| stage | N=50 ms/share | N=200 ms/share | N=400 ms/share | growth |
| --- | ---: | ---: | ---: | ---: |
| `actor_total` | 2.209401 / 97.9% | 7.682919 / 98.0% | 17.777936 / 99.7% | 8.05x |
| `editor_total` | 2.198340 / 97.4% | 7.670506 / 97.8% | 17.763408 / 99.6% | 8.08x |
| `editor_transaction_build` | 0.021590 / 1.0% | 0.028303 / 0.4% | 0.031499 / 0.2% | 1.46x |
| `application_request_build` | 0.007073 / 0.3% | 0.008015 / 0.1% | 0.008666 / 0.0% | 1.23x |
| `application_outcome` | 0.003266 / 0.1% | 0.003026 / 0.0% | 0.004178 / 0.0% | 1.28x |

The every-page branch had the same dominant nested pair. Its top three absolute
foreground growth rows were `actor_total` (+16.765406 ms, 8.04x),
`editor_total` (+16.760665 ms, 8.10x), and `editor_transaction_build`
(+0.013044 ms, 1.33x). `actor_total` and `editor_total` are nested aggregate
views, not additive costs.

| forced-drain stage (last drain) | N=50 ms | N=200 ms | N=400 ms | growth |
| --- | ---: | ---: | ---: | ---: |
| `authenticate` | 0.109534 | 0.154166 | 0.222242 | 2.03x |
| `archive_publication` | 0.426361 | 0.604893 | 0.599723 | 1.41x |
| `engine_acceptance` | 1.750449 | 6.086454 | 10.608326 | 6.06x |
| `tail_and_sqlite` | 7.054993 | 26.042400 | 61.293404 | 8.69x |
| `projection_adoption` | 5.231760 | 19.248655 | 46.140208 | 8.82x |
| `authorship_receipt` | 0.009798 | 0.017051 | 0.026770 | 2.73x |
| `provider_publication` | 0.000050 | 0.000050 | 0.000050 | 1.00x |
| `checkpoint` | 0.005049 | 0.008536 | 0.013325 | 2.64x |
| `total` | 14.759271 | 52.190758 | 118.955798 | 8.06x |

The top three growing forced-drain rows by absolute increase are
`tail_and_sqlite` (+54.238411 ms), its nested `projection_adoption`
(+40.908448 ms), and `engine_acceptance` (+8.857877 ms). The only nonzero save
counter in the compact table, `response_target_exact_dto_reparses`, stayed at
one at N=50, N=200, and N=400 for both cadences. P0 item 2—the redundant
per-save re-plan—does not show a growing signature at this commit; it had already
been cut. Projection adoption is still costly, but must not be relabeled as the
removed foreground re-plan.

## Costed terminal shared-read plan

The terminal design uses the existing projection database, existing schema, and
existing `SqliteGraphProjectionRead` for both backends. It does not add a second
projection, a schema, or backend-specific application twins.

| Planned class | Existing common-view wiring | Estimated production wiring | Equivalence/fallback tests |
| --- | --- | ---: | ---: |
| `Task` | fold the existing Direct sparse route and Managed task reads behind the common plan executor | 40–80 lines | 100–160 lines |
| `PageRef` | `page_referrer_candidates_after` plus exact page-self membership; Direct leases its current projection read | 40–60 | 100–160 |
| `Property` | `block_property_candidates_after`; retain oracle fallback while projection is unready | 25–40 | 80–120 |
| `PageProperty` | replace Managed’s full facet enumeration with targeted `properties_named_after`; same read for Direct | 25–40 | 80–120 |
| `PageTags` | use `tags_after`/targeted `properties_named_after`; same read for Direct | 20–35 | 70–110 |
| `Page` | exact normalized-name navigation read instead of inventory filtering | 15–25 | 60–90 |
| `Namespace` | namespace navigation read instead of inventory filtering | 15–25 | 60–90 |
| `Journal` / journal `Between` | common navigation inventory plus kind/date filter; no new schema | 20–35 | 80–120 |
| complete `And` / `Or` | compose common candidate sets; fall back unless the plan is complete | 40–70 | 120–180 |
| proposed literal `Content` | `plain_text_candidate_pages_after`, with Unicode/token-safety fallback | 30–50 | 100–160 |

Factoring the Managed plan-to-read switch into one backend-neutral executor and
adding a Direct projection-read lease is estimated at another 80–120 lines of
production code and 120–180 lines of shared readiness/error tests. The four
immediate fact-complete classes named by the dossier—PageRef, block Property,
PageProperty, and literal Content—are therefore approximately 180–260 lines of
plan-to-read wiring, consistent with the prior “~200 lines” estimate, plus
360–560 lines of oracle-equivalence, readiness, stale-generation, and bounded
result tests. These estimates reuse existing reads and lowering; they do not
price a schema change because none is required.

Each class flips only after oracle-versus-indexed equivalence passes on fixture
and anonymized graphs. Unready, stale, unsupported-token, incomplete boolean,
regex, and negative-only cases continue to the oracle walk. Per-backend
evaluation is explicitly interim; it is not the terminal architecture.

### Ranked sequencing

| rank | class | fixture query containment | invalidated median | frequency × cost | decision |
| ---: | --- | ---: | ---: | ---: | --- |
| 1 | PageRef | 4 | 8.950936 ms | 35.804 | first new class |
| existing | Task | 8 | 1.652907 ms | 13.223 | already SQL-served; separately fix readiness |
| dependency | boolean composition | 3 | 1.393947 ms | 4.182 | compose only after its positive operands |
| later | measured property/tag/text classes | 0 | 0.299–1.482 ms | 0 | retain measurements; corpus gives no frequency priority |

`Page` appeared once but did not have a dedicated timing row, so it is not given
an invented frequency-times-cost score. The anonymized graph contributes scale
to timing but no query-frequency vote because its census contained zero query
forms.

## Reproduction record

Machine: `Linux caman 6.1.0-45-amd64 #1 SMP PREEMPT_DYNAMIC Debian
6.1.170-1 (2026-04-30) x86_64 GNU/Linux`, 12 logical CPUs. The machine had zero
interactive users. The final query run began at load averages
`0.03 0.36 0.79`; an earlier query observation was `1.73 1.26 1.18`, and the
isolated A4 measurement session began at `1.07 1.14 1.11`. Benchmarks ran
release-only, ignored, single-threaded, on a quiet machine, with medians of 3.

Commands, verbatim:

```text
rtk node scripts/harvest-query-census.mjs --graph tine-test=/home/koutecky/research/tine-test --graph kitchen-sink=src/fixtures/kitchen-sink.md --graph org-graph=/home/koutecky/research/org-graph --graph anonymized=/home/koutecky/research/logseq-anonymized
rtk node scripts/harvest-b4-query-attribution.mjs --graph /home/koutecky/research/logseq-anonymized --runs 3 --rounds 9 --facet-sizes 1000,4000
rtk node scripts/harvest-a4-save-attribution.mjs --runs 3 --checkpoints 50,200,400 --cadences never,every
```

The two drivers invoke `cargo test -p tine-core --release --lib … -- --ignored
--nocapture --test-threads=1`. The census is count-only. No source content from
the anonymized corpus appears in this report.

After a second comparable baseline exists, the first recommended policy gate is
a discriminator ratio rather than an absolute host-dependent duration:
`sparse_task invalidated median / page_ref invalidated median <= 0.25`, together
with schema checks for all expected rows. Per-class absolute and facet-growth
gates should be added only after the corresponding class is flipped and its
baseline exists; no gate is added by this packet.
