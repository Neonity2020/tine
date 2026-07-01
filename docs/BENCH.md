# Perf bench — spotting regressions

Tine exists to be fast at the thing OG Logseq is slow at: **loading and scrolling
a large page on a modest machine.** `npm run bench` measures that objectively and
repeatably, so a refactor that quietly makes it slower gets caught instead of
shipped. It is a **regression detector, not a profiler** — it answers "did this
get meaningfully worse?", not "how many microseconds does X take."

```bash
source scripts/env.sh
npm run build            # the bench serves dist/ via `vite preview` (a PROD build)
npm run bench            # measure + compare to scripts/bench-baseline.json
npm run bench -- --update  # re-record the baseline (do this on a QUIET machine)
```

Run the node script directly — **no `timeout` wrapper** (it would orphan the vite
child; the script SIGKILLs vite itself in a `finally`).

## What it measures

Headless Chromium drives the mock backend's gated **2000-block "Big" page**
(`?big` in `src/mock.ts`). Each metric is timed **in-page** with `performance.now`
and reported as the **min of K=8** runs (least noise), after one discarded warmup.
It boots once and measures **warm navigations** (journals ↔ Big) so the numbers
reflect the app's own mount/render cost, not per-reload JIT + WASM-compile jitter.

| metric      | what | character |
|-------------|------|-----------|
| `bigLoad`   | switch to the 2000-block page → stable render | **coarse** — mounting 2000 Solid components is GC-sensitive; ~10–15% run-to-run noise, more on a loaded machine. Catches gross regressions (a doubling), not micro-drift. |
| `scrollBig` | scroll the Big page to the bottom → settle (blocks render on demand) | tight (~a few %). |
| `parseStats`| `window.__tineParseStats` after a cold Big open — cold parses ≈ blocks actually parsed | exact. **~12 on a 2000-block page = block virtualization is intact.** If this jumps toward 2000, lazy-body rendering broke. |

`parseStats` works in the prod build because the bench sets `window.__tineBench`
before boot (see the `statsEnabled()` opt-in in `src/render/parse.ts`); for normal
users the counter is dead-code-eliminated.

## Tier 2 — the calibration normalizer ("code, or machine?")

A fixed, deterministic CPU loop (`calib` ms) measures this machine's cost for a
unit of work right now. Every app metric is reported **raw** and **normalized =
raw / calib**, and the baseline stores the normalized numbers — so a baseline
recorded on a fast box still roughly holds on a slow one.

- If `calib` is **> 1.5× its baseline**, the machine is throttled/loaded → the run
  prints "UNRELIABLE, re-run cooler" and does **not** fail.
- Otherwise each normalized metric is compared to the baseline; anything more than
  **30%** worse is flagged `REGRESSED` and the run exits non-zero (so this can gate
  CI later). The 30% sits above `bigLoad`'s noise floor on purpose.

The baseline (`scripts/bench-baseline.json`) records the machine it was taken on;
`--update` on that same machine when quiet is the most meaningful comparison.

## Deferred (not built here)

- **Tab-switch** and **per-keystroke typing** metrics — dropped from this pass
  because both are dominated by harness/IPC noise (a flaky metric is worse than
  none). Revisit with an in-page driver if they earn their keep.
- **Tier 3** — a CI gate wired to `npm run bench` and a live in-app perf overlay.
