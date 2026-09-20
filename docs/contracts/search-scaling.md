# Search scaling budget

This records the approved 2026-09-20 clarification of the compact-projection
search decision, §10 A2/A4. The executable policy is
`scripts/projection-budget-policy.json`. The canonical private decision note
lives outside the application Git repository; this checked-in counterpart
keeps the decision and policy in one commit.

## Independent growth dimensions

All timings are p95 from the same source, probe, query workload, augmentation,
sample count and presentation limits. Measure combined page/block search with
limits 100/100. Search membership, ordering and the interactive window do not
change to satisfy these gates.

- **Block growth:** 60,000 to 600,000 blocks with the measured navigation-name
  inventory held fixed. Combined broad and sparse search must each stay within
  **1.5×**, with no additional noise margin.
- **Both growing:** retain the 600,000-block, 10,000-page fixture. Combined broad
  search has a hard ceiling of **49.28 ms** and sparse search **41.69 ms**.
  These are the approved 44.8 / 37.9 ms reference values plus the existing
  **10%** policy margin, exactly once. Baseline recording must not ratchet
  these caps; no second margin applies during evaluation.
- **Name growth:** hold blocks at 60,000 while pages grow from 1,000 to 10,000.
  Measure exhaustive page-name search with the broad and no-hit needles.
  Divide each latency ratio by the actual measured navigation-name inventory
  ratio; the maximum must be at most **1.10**. Exactly tenfold names therefore
  permits at most 11× latency. Actual counts include fixed tags and augmentation
  names, so report their small constant contribution explicitly.

Require evidence that each held-fixed dimension is unchanged. Missing reports,
wrong dimensions or inconsistent provenance cannot pass by omitting a row.
Retain the both-growing absolute times and ratios in the report.

## Preserved requirements

The names dictionary remains an exhaustive fuzzy scan, explicitly linear in
name inventory under A4. Keep the page-heavy T2-pages measurement and exact
older-name retention proof. T2-nohit and T2-fp retain their named diagnostic
exceptions. All other absolute/baseline budgets remain unchanged, including
the synthetic10k T2 absolute ceiling. This clarification does not certify
unmeasured workloads or authorize another search implementation or index.
