# 0067. The public Guide is a live read-only export

- **Status:** Accepted
- **Date:** 2026-09-20

## Context

The public Guide was generated through Tine's older HTML publisher while the
product's richer publication path shipped the ordinary frontend over a baked,
read-only snapshot. This left the most visible Tine publication on the legacy
presentation and stopped it from exercising the feature it documented.

The public Guide needs a stable page URL, a no-JavaScript fallback, and a
reproducible checked-in artifact so GitHub Pages can deploy it without a Tine
backend.

## Decision

`npm run docs:build` builds the current frontend and publishes the onboarding
demo graph through the read-only app exporter. The app opens the real
`Welcome to Tine` source page; it does not add a synthetic query-export home.
The existing HTML site remains in the same folder as the `file://`, `?static`,
and no-JavaScript fallback.

The checked-in site includes `app/`, `snapshot.json`, and the filtered frontend
bundle. The Guide build identifies itself as `public-guide` and pins the
frontend's informational build timestamp to the reproducible-build epoch,
while `snapshot.json` records the actual export time. Freshness comparison
ignores only that timestamp value. A semantic validator requires the redirect,
published-snapshot marker, Guide identity,
real home page, nonempty page set, and read-only pages. CI regenerates the same
artifact and compares every other byte.

GitHub Pages continues to deploy the checked-in `website/` directory. Changing
the canonical generator means both an explicit Guide rebuild and every future
CI freshness check use the live path.

## Consequences

- The public Guide exercises application boot, snapshot loading, navigation,
  baked queries, and the published backend on every regeneration.
- Static URLs and the HTML fallback remain available.
- Frontend changes can make `website/guide/` stale even when Guide Markdown did
  not change; `npm run docs:build` is therefore required for either kind of
  change.
- The generated Guide grows from the static files to about 11 MB because it
  carries the frontend bundle and a 337 KB snapshot.

**Unit cost:** one filtered frontend bundle and one snapshot per generated
Guide, currently 128 files and about 11 MB total; no per-edit artifacts and no
runtime server or database.
