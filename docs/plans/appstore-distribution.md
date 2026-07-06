# Plan — App-store distribution (F-Droid + Google Play)

**Status:** not started (Martin promoted both to the top of Next, Jul 6 2026). Ship
order below. This is the resume-from-cold spec.

## Eligibility snapshot (verified Jul 6 2026)

Tine's Android build is **F-Droid-clean** — no blockers on the licensing/dependency
side, so the work is packaging + process, not code surgery:

- **License:** AGPL-3.0 (`LICENSE`) — FOSS, F-Droid-eligible.
- **No proprietary deps.** Android Gradle deps are all Apache-2.0: `androidx.*` and
  `com.google.android.material:material` (Material Components — FOSS despite the
  `com.google` package name, github.com/material-components/material-components-android).
  No Firebase / GMS / play-services / crashlytics. (The other `com.google.*` strings in
  `GraphFolderPickerPlugin.kt` / `WryActivity.kt` are URI-authority / webview-package
  constants, not dependencies.)
- **No runtime self-update / telemetry on Android.** `tauri-plugin-updater` is under
  `[target.'cfg(not(any(target_os="android",target_os="ios")))'.dependencies]` in
  `src-tauri/Cargo.toml` — desktop-only. So no F-Droid `UpdateCheck` AntiFeature and no
  non-free-network concern.
- **Local-first, no data collection** → the Play "Data safety" form and F-Droid
  AntiFeatures are both trivial (declare nothing collected).

Current build reality: `release.yml`'s `android` job builds a **signed APK** (arm64) on
`v*` tags (`npx tauri android build --target aarch64 --apk`), keystore from GH secrets
`ANDROID_KEYSTORE_*`. Play needs an **AAB**; F-Droid builds from source (ignores our APK)
unless we opt into reproducible-build verification.

---

## Track A — F-Droid (do first; cheapest real distribution)

Two sub-paths. **A1 (self-hosted repo)** is hours of work and fully in our control —
do it first for immediate distribution. **A2 (main f-droid.org repo)** is the real goal
(searchable in the F-Droid client) but is weeks of build-recipe iteration because Tauri
(Rust + npm) must build **offline** on F-Droid's servers.

### A1 — Self-hosted F-Droid repo (fast path, full control)

Users add a repo URL (or scan a QR) in the F-Droid client; we publish our own
**self-signed** APKs on our own cadence, no review.

1. `pipx install fdroidserver` on the build box (or a laptop). Needs `apksigner`
   (Android build-tools, already installed per `tine-android-build`) + `aapt`.
2. `mkdir tine-fdroid && cd tine-fdroid && fdroid init` — generates a repo keystore
   (BACK IT UP; it's the repo's identity, distinct from the app signing key).
3. Drop the signed release APK(s) into `repo/` (e.g. `Tine_0.4.x_android-arm64.apk`
   from the GitHub release), then `fdroid update --create-metadata` → builds `index-v2`.
4. Add richer metadata in `metadata/dev.tine.app.yml` (Name, Summary, Description,
   License AGPL-3.0-only, SourceCode, IssueTracker, Categories) + re-run `fdroid update`.
5. Host the `repo/` directory statically. Easiest: a `fdroid/` path on **tine.page**
   (GitHub Pages) or a `gh-pages` branch — it's just static files. Add the repo URL +
   QR to the website's download section and README.
6. Automate: a CI step (or a small script) that, on each `v*` tag, downloads the release
   APK, runs `fdroid update`, and commits/pushes `repo/`. Repo signing key lives in a
   secret.

**Deliverable:** "Add our repo to F-Droid" instructions on tine.page. Users get
auto-updates through the F-Droid client from our own signature.

### A2 — Main f-droid.org repository (the real goal; slow)

F-Droid builds from source on their infra with **no network access** during the build.
The Tauri Rust+npm toolchain is the whole difficulty.

1. **Make the build reproducible + offline-buildable** (do in our repo first):
   - Pin toolchains: `rust-toolchain.toml` (exact Rust), and an explicit Node version.
   - Vendor Cargo: `cargo vendor` + `.cargo/config.toml` `[source.crates-io] replace-with`.
   - npm offline: `package-lock.json` is committed already; the F-Droid recipe prefetches
     and runs `npm ci --offline`.
   - Verify a clean build with NO network locally (drop the box offline / use a netns)
     end-to-end: `npm ci` → `tauri android build --apk`.
2. **Fork `gitlab.com/fdroid/fdroiddata`**, add `metadata/dev.tine.app.yml`:
   - Header: `Categories`, `License: AGPL-3.0-only`, `AuthorName`, `SourceCode`,
     `IssueTracker`, `Changelog`.
   - `Builds:` one entry per version — `versionName`/`versionCode`, `commit: v0.4.x`,
     `subdir: src-tauri/gen/android`, `sudo:`/`prebuild:` to install Node + `npm ci`
     (and `cargo vendor` if not committed), `ndk:` (r26 per `tine-android-build`),
     `rust:` toolchain, `build:` = `tauri android build --apk`, `output:` = the APK path
     under `.../release/`.
   - `AutoUpdateMode: Version` + `UpdateCheckMode: Tags` so new tags auto-propose builds.
   - **Reproducible builds (recommended):** add `Binaries:` pointing at our GitHub-release
     APK URL. If F-Droid's build byte-matches ours, F-Droid ships **our** signature — so a
     user who sideloaded the GitHub APK can update in place. Requires our release build to
     be deterministic (fixed timestamps, sorted zip entries).
3. **Validate before the MR:** run F-Droid's own build via `fdroidserver`
   (`makebuildserver` VM or the official CI image) — `fdroid build -v -l dev.tine.app` —
   and iterate until it builds clean offline. This is where most of the time goes.
4. **Open the MR** to `fdroiddata`, respond to the reviewer. After merge, first build can
   take days–weeks; thereafter each tag auto-builds.

**Gotcha:** many Tauri apps stall on A2's offline build. If A2 drags, A1 already covers
"distributed on F-Droid" — keep shipping via A1.

---

## Track B — Google Play

More friction than F-Droid for a solo/new account. Start the account + testing gate
EARLY (they run on wall-clock, not effort).

1. **Account:** create a Google Play Console account ($25 one-time). Complete **identity
   verification** (personal account: government ID; can take days). **New personal
   developer accounts must run closed testing with ≥12 testers opted-in for 14
   continuous days before production is unlocked** — start this the moment the account
   exists; it's the long pole.
2. **Build an AAB, enroll in Play App Signing:**
   - Add an `--aab` variant to the `release.yml` android job (`tauri android build
     --aab`; output `app-universal-release.aab`). Keep the APK for GitHub/F-Droid.
   - Enroll in **Play App Signing**: Google holds the app-signing key; we sign the AAB
     with an **upload key** (can reuse the existing `tine-release.jks` as the upload key).
   - Target **API 35** (Play's current minimum for new apps); `minSdk 24` is fine.
3. **Create the app** in Console (package `dev.tine.app`, unique — grab it before anyone):
   - Store listing: title, short + full description, app icon 512×512, feature graphic
     1024×500, ≥2 phone screenshots (reuse/adapt the website shots).
   - **Privacy policy URL** (required) — host a short one on tine.page.
   - **Data safety** form: local-first, no data collected/shared → declare nothing.
   - Content rating questionnaire, target audience, ads = none, government-app = no.
4. **Release rollout:** upload AAB → Internal testing → Closed testing (the 12-tester /
   14-day gate) → Production. Submit for review.

**Cost/timeline:** $25 + identity verification (days) + the 14-day testing gate. The AAB
switch + listing are ~a day of work; the calendar gate is the real wait.

---

## Recommended sequence

1. **A1 self-hosted F-Droid repo** — hours; ships a real, auto-updating F-Droid channel now.
2. **B1 Play account + identity verification + start the 12-tester/14-day closed test** —
   kick off early because it's calendar-bound; do the AAB switch + listing in parallel.
3. **A2 main f-droid.org MR** — the real F-Droid goal; weeks of offline-build iteration.

## Docs to update when shipping

- README + `website/` download section (add F-Droid repo/badge, Play badge when live).
- `CHANGELOG.md` when each channel goes live.
- Remove the shipped item(s) from `docs/BACKLOG.md` Next.
