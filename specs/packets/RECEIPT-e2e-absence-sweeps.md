# Packet E2E-AS receipt — absence-sweep surfacing UX

Date: 2026-08-28

Branch: `batch/e2e-absence-sweeps`

Base: `71cbdf153fbcc0e698e58391d05f1719e728c15d`

Candidate build commit: `d9113aa4dbdaedec7317b2b8dced31e5cddf69f5`

Harness correction commit: `c35f2fc4a18c6d1f8faf384e981a249c3671c6d2`

Outcome: the native journey and contracts entry are implemented, but the
journey is **quarantined/advisory**. Both permitted debugging runs reached the
real surfaced Tier-3 sweep with all eight members and three actions. Neither
reached Restore: the first stopped on a toast-lookup harness race; the corrected
second run found that the panel had disappeared before the semantic close
lookup while repeated managed `RecoveryBlocked` / `ProjectionDrain` error
toasts were visible. The two-red stop-loss is exhausted, so burn-in was not
started (0/10).

No production file, private corpus, alternate instruction copy, or prohibited
brain path was read or changed. The fixture contains only generated synthetic
Markdown.

## 1. Journey and exact assertions

File: `scripts/e2e-absence-sweeps.mjs`.

The script self-provisions its display, launches Openbox, starts
`tauri-driver` + WebKitWebDriver, and creates an isolated graph/XDG pair. The
graph has 20 synthetic Markdown pages. It activates Tine-managed storage
through Settings and the native confirmation, cleanly exits Tine, deletes eight
known page files while Tine is closed, then reopens the exact graph and private
state.

The ordered semantic assertions are:

1. Poll until either the stable `8 pages were deleted together` warning plus
   **Review**, or the persistent `8 deleted pages` / **Review** control, is
   visible. There is no fixed scenario sleep.
2. Open the accessible **Deleted page recovery** surface from Review/dock and
   assert **Tier 3**, exactly `8 deleted pages`, **Waiting for your decision**,
   the eight unique page names exactly once each, and the exact live actions
   **Restore**, **Re-apply**, and **Keep deletion**.
3. Close the panel, dismiss the matching group-deletion toast if it remains,
   reopen from the persistent Review surface, and reassert the waiting state,
   members, and all three actions. This is the no-dispose-on-dismiss oracle.
4. Invoke **Restore** and poll until every deleted file exists with its exact
   original synthetic bytes. Then require the panel to say **Restored**, retain
   all eight members, and expose none of the three live actions.
5. Close history, navigate through the user-facing Ctrl-K page switcher to a
   restored page, and require its exact synthetic marker in the visible page.
6. Cleanly exit. On any failure, write the tested commit, phase, expected and
   observed semantic result, classification, screenshot/DOM/debug evidence,
   and the structured scenario receipt.

The script intentionally does not assert pixels, geometry, palette, animation,
toast wording beyond the stable group-deletion family, exact DOM nesting, or
internal sweep/page IDs.

## 2. Necessity evidence

The native journey closes the exact gap recorded by C-5b §7: C-5b supplied
render/component proof and screenshots, but no real-app path across managed
activation, closed-app disk deletion, native reopen, disposition, projection,
and navigation.

- Surfacing, members, and actions pair with C-5b render test `lists a surfaced
  sweep and its member pages` plus the changed-snapshot subscription proof. The
  native polling oracle would time out if neither warning nor persistent Review
  surface existed, and the panel assertion would fail if the tier/count/member
  or action state were absent. Both native attempts observed this contract.
- No-dispose-on-dismiss pairs directly with C-5b render test `never disposes a
  sweep when its toast or panel is dismissed`. The native re-open assertion
  requires the same durable waiting state and actions after both presentation
  closes; a dismissal wired to a disposition cannot pass it.
- Restore initiation pairs with C-5b render test `shows a failed Restore cause
  and re-runs Restore explicitly`, which proves the control calls Restore with
  the exact sweep ID. The new disk-byte and navigation oracles are necessary
  because that render test cannot prove the native command, managed actor, and
  Markdown projection complete. If any deleted file is absent or has different
  content, the bounded exact-byte predicate remains false; if the application
  does not ingest the restored page, Ctrl-K/title/content assertions fail.
- Disposed history pairs directly with C-5b render test `keeps a disposed sweep
  visible with its deliberate disposition`. The native oracle additionally
  requires all eight members to remain present and all three live controls to
  be absent, so a hidden or still-actionable disposed record cannot pass.

No production behavior was deliberately broken in this lane. The render-test
pairing and causal real-boundary predicates above provide the packet's allowed
necessity argument; the native run did not reach assertions 3-5 before the
stop-loss.

## 3. Native runs and failure capsules

The app was built through the provenance-producing sanctioned release script
with `TINE_FAST_LOCAL_BUILD=1`; its deployment destination was redirected to
`artifacts/e2e-absence-sweeps/candidate` inside this worktree. The exact
candidate and its build receipt were unchanged across both runs. The second run
used only a harness-script correction, so the candidate build receipt still
names its parent `d9113aa4`; no production/frontend input changed.

### Attempt 1 — harness

- Tested harness/product commit: `d9113aa4dbdaedec7317b2b8dced31e5cddf69f5`.
- Reached: clean managed activation and close, eight-file external deletion,
  managed reopen, warning Review surface, complete Tier-3 panel with eight
  members, waiting state, and all three controls.
- Expected next outcome: close/dismiss presentation without disposing, then
  reopen the waiting sweep.
- Observed: the harness cached the set of Dismiss elements, computed an index
  against a later DOM, and WebDriverIO rejected the out-of-bounds element. This
  was corrected by one atomic semantic lookup; no product code changed.
- Classification: **harness**.

### Attempt 2 — ambiguous product/harness debt

- Tested harness commit: `c35f2fc4a18c6d1f8faf384e981a249c3671c6d2`;
  exact unchanged candidate from `d9113aa4`.
- Reached: the same complete initial semantic surface. This time the first
  surfacing snapshot observed both warning+Review and the dock.
- Expected next outcome: close the panel through its accessible close control.
- Observed: immediately after the successful initial panel snapshot, the panel
  was absent and the close lookup failed. Failure DOM/screenshot showed the
  persistent `8 deleted pages / Review` dock plus a stack of repeated product
  diagnostics: `RecoveryBlocked("retained recovery for batch ... is stuck at
  ProjectionDrain: managed projection mutation has no turn-derived attempt
  identity")`. The debug log shows managed open itself completed active, but
  does not contain the frontend diagnostic detail.
- Classification: **ambiguous**. The close lookup needs no production test hook
  in the ordinary surface (the accessible control exists in source and was
  rendered during C-5b); the open question is whether managed recovery churn
  reset presentation or the harness opened a transient surface and sampled it
  before the reset.

Per the packet's house rule, no third debugging run was made.

## 4. Burn-in and catalog

Burn-in: **0/10**. It was not started because no green qualifying run preceded
it. There is therefore no proposal to make this journey a blocking gate.

`tests/ui-regressions/e2e-contracts.json` now contains two blocking contract
classes for the intended behavior (`core-operation` and `stateful-ux`) but marks
the scenario `stability: "quarantined"` with the dated two-run failure
signature. In runner semantics it remains advisory. `scripts/run-e2e.mjs` was
not edited because it is outside this packet's write set; after the debt is
resolved and the unchanged candidate passes 10/10, the manager may register it
and promote stability.

The catalog checker passes with the quarantined entry.

## 5. Artifacts

Candidate/build provenance:

- `artifacts/e2e-absence-sweeps/candidate`
- `artifacts/e2e-absence-sweeps/candidate.build.json`
- `target/release/tine.build.json`

Attempt 1:

- `artifacts/e2e-absence-sweeps/attempt-1/absence-sweeps-receipt.json`
- `artifacts/e2e-absence-sweeps/attempt-1/surfaced-panel.png`
- `artifacts/e2e-absence-sweeps/attempt-1/failure.png`
- `artifacts/e2e-absence-sweeps/attempt-1/failure-dom.html`
- `artifacts/e2e-absence-sweeps/attempt-1/tine-debug.log`
- `artifacts/e2e-absence-sweeps/attempt-1/initial-tauri-driver.log`
- `artifacts/e2e-absence-sweeps/attempt-1/managed-reopen-tauri-driver.log`
- `artifacts/e2e-absence-sweeps/attempt-1/openbox.log`

Attempt 2 has the same filenames under
`artifacts/e2e-absence-sweeps/attempt-2/`.

## 6. Exact commands and focused checks

Setup/build (the pre-existing `node_modules` symlink was removed without
traversal before `npm ci`):

```text
rtk rm node_modules
rtk bash -lc 'source scripts/env.sh && npm ci'
rtk bash -lc 'source scripts/env.sh && TINE_FAST_LOCAL_BUILD=1 TINE_DEPLOY_DEST="$PWD/artifacts/e2e-absence-sweeps/candidate" ./scripts/deploy.sh'
```

Native attempts:

```text
rtk bash -lc 'source scripts/env.sh && E2E_ARTIFACT_DIR="$PWD/artifacts/e2e-absence-sweeps/attempt-1" TINE_APP="$PWD/artifacts/e2e-absence-sweeps/candidate" node scripts/e2e-absence-sweeps.mjs'
rtk bash -lc 'source scripts/env.sh && E2E_ARTIFACT_DIR="$PWD/artifacts/e2e-absence-sweeps/attempt-2" TINE_APP="$PWD/artifacts/e2e-absence-sweeps/candidate" node scripts/e2e-absence-sweeps.mjs'
```

Focused harness checks:

```text
rtk node --check scripts/e2e-absence-sweeps.mjs
rtk node scripts/check-ui-regression-catalog.mjs
rtk git diff --check
```

All focused harness checks pass. The build completed successfully with existing
Rust warnings only.

## 7. Deviations and open questions

- Deviation from the desired pass path: steps 3-5 were not completed and the
  10-run burn-in was not attempted because the two-red stop-loss fired.
- No permitted production or shared harness-library change was needed or made.
- Open question/blocker: identify whether the repeated retained-recovery
  `ProjectionDrain` failure is a product defect that resets the recovery panel,
  or whether the harness must wait for a stable post-open managed state before
  interacting with the panel. Resolve that classification before another run.
- If it is a product defect, its fix is outside this harness-only packet and
  must be assigned to an authorized production lane. If it is harness debt,
  repair only the semantic readiness/close interaction, then rerun once and, if
  green, burn in the exact unchanged binary 10 times.
