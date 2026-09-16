#!/usr/bin/env node
// Research fixture for GH #149 (reopened 2026-09-16 on 0.6.982): "Light theme
// makes title bar not visible" on Android.
//
// Deterministic source-level reproduction of the system-bar strip/icon pairing
// when no Android device or emulator is available. It parses the REAL Kotlin
// and resource XML at a git revision and answers, for every combination of
// Tine's own theme and the Android system night setting, whether the status
// bar is readable at steady state (after SystemBarAppearance.apply has run).
//
// Why this is the defect's shape: since 0.6.981 (945337d0, GH #205) the
// Activity content root is padded by the system-bar/cutout insets, so the strip
// behind the status bar is painted by the WINDOW background, which the
// DayNight theme resolves from the ANDROID night setting. The bar ICONS follow
// TINE's own theme via SystemBarAppearance. When the two disagree, the icons
// land on a strip of matching polarity and the bar goes blank:
//   - GH #467 (v0.6.981): Tine dark + phone light -> white icons on white strip.
//   - GH #149 (v0.6.982): Tine light + phone dark -> dark icons on #1A1B1E.
// ef7a5fcd (shipped in v0.6.983) made one authority paint both.
//
// Usage:
//   node scripts/research/tine-149-android-systembar-contrast.mjs            # HEAD
//   node scripts/research/tine-149-android-systembar-contrast.mjs v0.6.982   # defect revision
//
// Exits 1 when any steady-state cell is unreadable (contrast < 3:1, the WCAG
// threshold for graphical objects). Run against v0.6.982 to see it fail for
// the same reason the reporter's phone fails; run against HEAD to see it pass.
//
// Model notes / deliberate approximations:
//   - Icon colours are taken as the pure endpoints #000000 (dark icons) and
//     #FFFFFF (light icons). Android tints them slightly, which only ever
//     lowers contrast, so the endpoint is the optimistic case: if the endpoint
//     is unreadable, the real bar is worse.
//   - Pre-0.6.981 (no insets padding) the strip was painted by the PAGE, which
//     follows Tine's theme; the window colours are used as stand-ins for the
//     page's two theme backgrounds. Only the polarity of that pairing matters
//     for the verdict, not the exact page colour.

import { execFileSync } from "node:child_process";
import process from "node:process";

const REPO_FILES = {
  plugin: "src-tauri/gen/android/app/src/main/java/page/tine/app/SystemBarsPlugin.kt",
  activity: "src-tauri/gen/android/app/src/main/java/page/tine/app/MainActivity.kt",
  valuesColors: "src-tauri/gen/android/app/src/main/res/values/colors.xml",
  nightColors: "src-tauri/gen/android/app/src/main/res/values-night/colors.xml",
};

function gitShow(ref, path) {
  return execFileSync("git", ["show", `${ref}:${path}`], {
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
  });
}

/** Like gitShow, but null when the path does not exist at that revision
 *  (e.g. values-night/colors.xml does not exist at v0.5.9). */
function gitShowOrNull(ref, path) {
  try {
    return gitShow(ref, path);
  } catch {
    return null;
  }
}

function parseColor(xml, name) {
  const m = xml.match(new RegExp(`<color name="${name}">#([0-9A-Fa-f]{6,8})</color>`));
  if (!m) return null;
  const hex = m[1].slice(-6); // drop alpha
  return {
    r: parseInt(hex.slice(0, 2), 16) / 255,
    g: parseInt(hex.slice(2, 4), 16) / 255,
    b: parseInt(hex.slice(4, 6), 16) / 255,
  };
}

function srgbChannel(c) {
  return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

/** WCAG 2.x relative luminance. */
function luminance({ r, g, b }) {
  return 0.2126 * srgbChannel(r) + 0.7152 * srgbChannel(g) + 0.0722 * srgbChannel(b);
}

/** WCAG contrast ratio between two colours, e.g. icon vs strip. */
function contrast(a, b) {
  const la = luminance(a);
  const lb = luminance(b);
  const [hi, lo] = la >= lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

const BLACK = { r: 0, g: 0, b: 0 };
const WHITE = { r: 1, g: 1, b: 1 };

function readRevision(ref) {
  const plugin = gitShow(ref, REPO_FILES.plugin);
  const activity = gitShow(ref, REPO_FILES.activity);
  const valuesColors = gitShow(ref, REPO_FILES.valuesColors);
  const nightColors = gitShowOrNull(ref, REPO_FILES.nightColors);

  // Does the Activity pad the content root with the system-bar insets?
  // (Introduced by 945337d0 in v0.6.981; moves strip painting page -> window.)
  const insetsPadContentRoot =
    activity.includes("WindowInsetsCompat.Type.systemBars()") &&
    /view\.setPadding\(/.test(activity);

  // Icon rule from the v0.5.9 fix (e4a841b5): icons follow TINE's theme.
  const iconsFollowTine = plugin.includes("isAppearanceLightStatusBars = !dark");
  if (!iconsFollowTine) {
    throw new Error(
      `${ref}: cannot find the icon-appearance rule ("isAppearanceLightStatusBars = !dark") in ${REPO_FILES.plugin}`,
    );
  }

  // One-authority strip painter from ef7a5fcd (v0.6.983): the window
  // background is set from the same `dark` flag, via colors with no
  // values-night variant.
  const stripFollowsTine =
    plugin.includes("setBackgroundDrawable") &&
    plugin.includes("if (dark) R.color.tine_system_bar_dark else R.color.tine_system_bar_light");
  const stripLight = parseColor(valuesColors, "tine_system_bar_light");
  const stripDark = parseColor(valuesColors, "tine_system_bar_dark");
  if (stripFollowsTine && (!stripLight || !stripDark)) {
    throw new Error(`${ref}: strip painter present but tine_system_bar_{light,dark} missing from values/colors.xml`);
  }
  // The one-authority rule is void if a night-qualified override exists.
  const nightOverride =
    (nightColors ?? "").includes("tine_system_bar_light") ||
    (nightColors ?? "").includes("tine_system_bar_dark");

  // DayNight window background resolved by the ANDROID night setting:
  // the pre-restore initial paint, and (before ef7a5fcd) the whole session.
  // Older revisions (v0.5.9) carry no tine_window_background -- there the page
  // painted the strip, so the stand-ins below carry polarity only.
  const windowLight =
    parseColor(valuesColors, "tine_window_background") ??
    parseColor(valuesColors, "tine_system_bar_light") ??
    WHITE;
  const windowDark =
    parseColor(nightColors ?? "", "tine_window_background") ??
    parseColor(valuesColors, "tine_system_bar_dark") ??
    { r: 26 / 255, g: 27 / 255, b: 30 / 255 };

  return {
    ref,
    insetsPadContentRoot,
    stripFollowsTine,
    nightOverride,
    stripLight,
    stripDark,
    windowLight,
    windowDark,
  };
}

/** Steady-state strip colour and icon polarity after SystemBarAppearance.apply. */
function steadyState(rev, tineDark, androidNight) {
  const iconsDark = !tineDark; // isAppearanceLightStatusBars = !dark
  let strip;
  let authority;
  if (rev.stripFollowsTine) {
    strip = tineDark ? rev.stripDark : rev.stripLight;
    authority = "tine";
  } else if (rev.insetsPadContentRoot) {
    // v0.6.981/v0.6.982: the window paints the strip and follows Android night.
    strip = androidNight ? rev.windowDark : rev.windowLight;
    authority = "android-night";
  } else {
    // v0.5.9: the page paints the strip (window colours used as stand-ins).
    strip = tineDark ? rev.windowDark : rev.windowLight;
    authority = "page(tine)";
  }
  if (rev.nightOverride) authority += "+night-override";
  return { strip, iconsDark, authority };
}

function main() {
  const ref = process.argv[2] ?? "HEAD";
  const rev = readRevision(ref);

  console.log(`GH #149 system-bar readability model at ${ref}`);
  console.log(`  content root padded by system-bar insets : ${rev.insetsPadContentRoot}`);
  console.log(`  one-authority strip painter (ef7a5fcd)   : ${rev.stripFollowsTine}`);
  console.log(`  values-night strip override present       : ${rev.nightOverride}`);
  console.log("");

  const rows = [];
  let worst = Infinity;
  let failed = false;
  for (const tineDark of [false, true]) {
    for (const androidNight of [false, true]) {
      const { strip, iconsDark, authority } = steadyState(rev, tineDark, androidNight);
      const icon = iconsDark ? BLACK : WHITE;
      const ratio = contrast(icon, strip);
      worst = Math.min(worst, ratio);
      const readable = ratio >= 3;
      if (!readable) failed = true;
      rows.push({
        tine: tineDark ? "dark" : "light",
        android: androidNight ? "dark" : "light",
        strip: strip === rev.stripDark || strip === rev.windowDark ? "#1A1B1E-ish (dark)" : "#FFFFFF-ish (light)",
        icons: iconsDark ? "dark" : "light",
        authority,
        ratio: ratio.toFixed(2) + ":1",
        verdict: readable ? "readable" : "UNREADABLE",
      });
    }
  }
  const pad = (s, n) => String(s).padEnd(n);
  console.log(
    pad("tine", 7) + pad("android", 9) + pad("strip", 22) + pad("icons", 7) + pad("authority", 26) + pad("contrast", 10) + "verdict",
  );
  for (const r of rows) {
    console.log(
      pad(r.tine, 7) +
        pad(r.android, 9) +
        pad(r.strip, 22) +
        pad(r.icons, 7) +
        pad(r.authority, 26) +
        pad(r.ratio, 10) +
        r.verdict,
    );
  }
  console.log("");
  if (failed) {
    console.log(
      `FAIL: worst-case icon/strip contrast ${worst.toFixed(2)}:1 is below 3:1 — ` +
        `the status bar is blank for that (tine theme, android night) combination. ` +
        `(GH #149 mechanism: strip and icons follow different authorities.)`,
    );
    process.exit(1);
  }
  console.log(`PASS: every combination keeps icon/strip contrast >= 3:1 (worst ${worst.toFixed(2)}:1).`);
}

main();
