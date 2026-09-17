#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import crypto from "node:crypto";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { reapProcessGroup } from "./lib/e2e-process-group.mjs";
import { describeMachine, machineSnapshot } from "./lib/e2e-machine-probe.mjs";
import { buildInputState, normalizedBuildInputState } from "./build-e2e-inputs.mjs";
import { freeLoopbackPort, windowsWebviewProfileSnapshot } from "./e2e-capabilities.mjs";
import { assertPromotionPlan, validatePromotionPlanForCheckout } from "./release-proof-reuse-lib.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const contractsPath = path.join(root, "tests/ui-regressions/e2e-contracts.json");
const suiteName = process.argv[2] ?? "linux-smoke";

/**
 * Flags, parsed strictly.
 *
 * `--scenario=` used to be read with a bare `argv.find(...)`, so an unknown
 * flag was silently ignored and a REPEATED `--scenario=` silently kept only
 * the first. Both cost debugging time during v0.6.984: a command that looked
 * like it honoured the flag ran a different set of scenarios and reported it
 * as if nothing were wrong. A typo must fail, loudly, before anything starts.
 */
const OPTIONS = {
  "--scenario": { kind: "value", usage: "--scenario=<id>            run one scenario from the suite" },
  "--validate-build-receipt-only": { kind: "flag", usage: "--validate-build-receipt-only  validate the build receipt and exit" },
  "--allow-harness-delta": { kind: "flag", usage: "--allow-harness-delta      permit uncommitted journey/test edits (never for a candidate)" },
  "--under-load": { kind: "optional", usage: "--under-load[=N]           run N CPU burners alongside the suite (default: one per core)" },
  "--pin-cpus": { kind: "value", usage: "--pin-cpus=<N>             confine the suite to CPUs 0..N-1 (Linux)" },
};

function optionsUsage() {
  return Object.values(OPTIONS).map((option) => `  ${option.usage}`).join("\n");
}

function optionFailure(message) {
  console.error(`run-e2e: ${message}\nknown options:\n${optionsUsage()}`);
  process.exit(2);
}

function parseOptions(args) {
  const values = new Map();
  for (const arg of args) {
    const equals = arg.indexOf("=");
    const name = equals < 0 ? arg : arg.slice(0, equals);
    const spec = OPTIONS[name];
    if (!spec) optionFailure(`unknown option ${arg}`);
    if (values.has(name)) optionFailure(`${name} given more than once`);
    if (spec.kind === "flag" && equals >= 0) optionFailure(`${name} takes no value`);
    if (spec.kind === "value" && equals < 0) optionFailure(`${name} requires a value`);
    values.set(name, equals < 0 ? true : arg.slice(equals + 1));
  }
  return values;
}

function positiveCount(values, name, fallback) {
  const raw = values.get(name);
  if (raw === undefined) return 0;
  if (raw === true) return fallback;
  const count = Number(raw);
  if (!Number.isInteger(count) || count < 1) optionFailure(`${name} needs a positive whole number, not ${JSON.stringify(raw)}`);
  return count;
}

const options = parseOptions(process.argv.slice(3));
const only = options.get("--scenario") === true ? undefined : options.get("--scenario");
if (only !== undefined && !only) optionFailure("--scenario= needs a scenario id");
const allowHarnessDelta = options.has("--allow-harness-delta");
const cpuCount = os.cpus().length;
const underLoad = positiveCount(options, "--under-load", cpuCount);
const pinCpus = positiveCount(options, "--pin-cpus", cpuCount);
if (pinCpus) {
  if (process.platform !== "linux") optionFailure("--pin-cpus needs taskset, which is Linux only");
  if (pinCpus > cpuCount) optionFailure(`--pin-cpus=${pinCpus} exceeds the ${cpuCount} CPUs this machine has`);
  if (spawnSync("taskset", ["--version"]).status !== 0) optionFailure("--pin-cpus needs taskset on PATH (util-linux)");
}
const pinnedCpuList = pinCpus ? Array.from({ length: pinCpus }, (_, index) => index).join(",") : null;

/**
 * Reproduce a "hosted-only" failure locally.
 *
 * A hosted runner differs from this machine mainly in speed: fewer cores, all
 * of them contended. During v0.6.984 two hosted assembly round trips (~90
 * minutes each) were spent guessing at a failure that `--pin-cpus=2
 * --under-load` reproduced in 40 seconds. The burners exit on their own if
 * this process dies, so an interrupted run cannot leave the machine loaded.
 */
const BURN = "const parent=process.ppid;let n=0;"
  + "for(;;){Math.sqrt(Math.random());if((n=(n+1)%5000000)===0&&process.ppid!==parent)process.exit(0);}";
let burners = [];
function startLoad() {
  if (!underLoad) return;
  burners = Array.from({ length: underLoad }, () => spawn(
    pinnedCpuList ? "taskset" : process.execPath,
    pinnedCpuList ? ["-c", pinnedCpuList, process.execPath, "-e", BURN] : ["-e", BURN],
    { stdio: "ignore", detached: false },
  ));
  console.log(`LOAD ${underLoad} CPU burners${pinnedCpuList ? ` pinned to CPUs ${pinnedCpuList}` : ""}`);
}
function stopLoad() {
  for (const burner of burners) { try { burner.kill("SIGKILL"); } catch {} }
  burners = [];
}
process.on("exit", stopLoad);
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => { stopLoad(); process.exit(1); });
const app = path.resolve(process.env.TINE_APP || path.join(root, process.platform === "win32" ? "target/release/tine.exe" : "target/release/tine"));
const artifactRoot = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(root, "test-results/e2e", suiteName));
const longFocusedWindows = suiteName === "windows-smoke" && only === "windows-direct-large-open";
const timeoutMs = Number(process.env.E2E_SCENARIO_TIMEOUT_MS || (longFocusedWindows ? 15 * 60_000 : 180_000));
const suiteStartedAt = new Date().toISOString();
function gitOutput(args) {
  const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
  if (result.status !== 0) throw result.error || new Error(`git ${args.join(" ")} failed: ${String(result.stderr || "").trim()}`);
  return result.stdout.trim();
}

const checkoutRevision = gitOutput(["rev-parse", "HEAD"]);
const receiptPath = process.env.TINE_E2E_BUILD_RECEIPT
  ? path.resolve(process.env.TINE_E2E_BUILD_RECEIPT)
  : `${app}.build.json`;
const e2eMode = process.env.TINE_E2E_MODE ?? "ordinary";
const promotionPlanPath = process.env.TINE_E2E_PROMOTION_PLAN
  ? path.resolve(process.env.TINE_E2E_PROMOTION_PLAN)
  : undefined;
let activePromotionPlan;
const allowedContractClasses = new Set(["exact-safety-interoperability", "core-operation", "stateful-ux", "flexible-presentation-heuristic"]);
const allowedStabilities = new Set(["stable", "burn-in", "quarantined"]);

if (!new Set(["ordinary", "release"]).has(e2eMode)) {
  throw new Error(`unknown TINE_E2E_MODE ${JSON.stringify(e2eMode)}; choose ordinary or release`);
}

// Rootless/container fallback for native focus tests. CI images normally
// install openbox + xdotool system-wide; a developer sandbox may instead keep
// their extracted Debian packages outside the repository. Discover that
// workspace-local bundle automatically so the documented npm command remains
// the complete gate rather than requiring a remembered shell incantation.
const portableDeps = path.resolve(process.env.TINE_E2E_DEPS_ROOT || path.join(root, "../.codex-deps/openbox/root"));
const baseProcessEnv = { ...process.env };
if (fs.existsSync(path.join(portableDeps, "usr/bin/openbox")) && fs.existsSync(path.join(portableDeps, "usr/bin/xdotool"))) {
  const lib = path.join(portableDeps, "usr/lib/x86_64-linux-gnu");
  baseProcessEnv.PATH = `${path.join(portableDeps, "usr/bin")}${path.delimiter}${baseProcessEnv.PATH || ""}`;
  baseProcessEnv.LD_LIBRARY_PATH = [lib, baseProcessEnv.LD_LIBRARY_PATH].filter(Boolean).join(path.delimiter);
  baseProcessEnv.XDG_CONFIG_DIRS = [path.join(portableDeps, "etc/xdg"), baseProcessEnv.XDG_CONFIG_DIRS || "/etc/xdg"].join(path.delimiter);
  baseProcessEnv.XDG_DATA_DIRS = [path.join(portableDeps, "usr/share"), baseProcessEnv.XDG_DATA_DIRS || "/usr/local/share:/usr/share"].join(path.delimiter);
}

// xprop lives in x11-utils rather than the Openbox/xdotool dependency bundle.
// Worktrees are one directory deeper than tine-master, so search both normal
// workspace layouts (or honor an explicit override) and add the first complete
// portable bundle to PATH. This keeps `npm run e2e:linux:release` sufficient in
// the rootless Codex sandbox instead of relying on a remembered PATH prefix.
const portableX11Deps = [
  process.env.TINE_E2E_X11_DEPS_ROOT,
  path.join(root, "../.codex-deps/x11-utils/root"),
  path.join(root, "../../.codex-deps/x11-utils/root"),
]
  .filter(Boolean)
  .map((candidate) => path.resolve(candidate))
  .find((candidate) => fs.existsSync(path.join(candidate, "usr/bin/xprop")));
if (portableX11Deps) {
  baseProcessEnv.PATH = `${path.join(portableX11Deps, "usr/bin")}${path.delimiter}${baseProcessEnv.PATH || ""}`;
}

const suites = {
  "plugin-revocation": [
    ["plugin-revocation", "scripts/e2e-plugin-revocation.mjs", {}],
  ],
  "plugin-graph-ownership": [
    ["plugin-graph-ownership", "scripts/e2e-plugin-graph-ownership.mjs", {}],
  ],
  "pdf-routes": [
    ["pdf-routes", "scripts/e2e-pdf-routes.mjs", {}],
  ],
  "og-parity-pilot": [
    ["og-parity-references", "scripts/e2e-og-parity-references.mjs", {}],
  ],
  "linux-smoke": [
    ["caret-agenda", "scripts/e2e-caret.mjs", { CARET_MODE: "agenda", CARET_LABEL: "runner" }],
    ["multigraph", "scripts/e2e-multigraph.mjs", {}],
    ["sheets", "scripts/e2e-sheets.mjs", {}],
  ],
  "linux-release": [
    ["wayland-app-id", "scripts/e2e-wayland-app-id.mjs", {}],
    ["caret-agenda", "scripts/e2e-caret.mjs", { CARET_MODE: "agenda", CARET_LABEL: "runner" }],
    ["caret-page", "scripts/e2e-caret.mjs", { CARET_MODE: "page", CARET_LABEL: "runner" }],
    ["click-caret", "scripts/e2e-clickcaret-repro.mjs", {}],
    ["block-select", "scripts/e2e-blockselect.mjs", {}],
    ["block-ref-count", "scripts/e2e-block-ref-count.mjs", {}],
    ["og-parity-references", "scripts/e2e-og-parity-references.mjs", {}],
    ["search-parity", "scripts/e2e-search-parity.mjs", {}],
    ["rename", "scripts/e2e-rename.mjs", {}],
    ["split-history", "scripts/e2e-split-history.mjs", {}],
    ["alias", "scripts/e2e-alias.mjs", {}],
    ["page-properties", "scripts/e2e-page-properties.mjs", {}],
    ["journal-format", "scripts/e2e-journal-format.mjs", {}],
    ["journal-future-feed", "scripts/e2e-journal-future-feed.mjs", {}],
    ["multigraph", "scripts/e2e-multigraph.mjs", {}],
    ["sheets", "scripts/e2e-sheets.mjs", {}],
    ["formula-builder", "scripts/probe-formula-builder.mjs", {}],
    ["selection-wrap", "scripts/e2e-selectwrap.mjs", {}],
    ["tag-autocomplete", "scripts/e2e-tag-autocomplete.mjs", {}],
    ["structured-paste", "scripts/e2e-structured-paste.mjs", {}],
    ["media", "scripts/e2e-media.mjs", {}],
    ["pdf-logseq", "scripts/e2e-pdf-logseq.mjs", { E2E_WINDOW_MANAGER: "openbox" }],
    ["pdf-routes", "scripts/e2e-pdf-routes.mjs", {}],
    ["pdf-scroll-resources", "scripts/e2e-pdf-scroll-resources.mjs"],
    ["pdf-ownership", "scripts/e2e-pdf-ownership.mjs", {}],
    ["plugin-revocation", "scripts/e2e-plugin-revocation.mjs", {}],
    ["plugin-graph-ownership", "scripts/e2e-plugin-graph-ownership.mjs", {}],
    ["external-assets", "scripts/e2e-external-assets.mjs", {}],
    ["capture", "scripts/e2e-capture.mjs", { E2E_WINDOW_MANAGER: process.env.E2E_WINDOW_MANAGER || "openbox" }],
    ["native-titlebar", "scripts/e2e-native-titlebar.mjs", { E2E_WINDOW_MANAGER: "openbox" }],
    ["page-file-actions", "scripts/e2e-page-file-actions.mjs", {}],
    ["print-security", "scripts/e2e-print-security.mjs", {}],
    ["block-embed", "scripts/e2e-block-embed.mjs", {}],
    ["compat-home-current-page", "scripts/e2e-compat-home-current-page.mjs", {}],
    ["sidebar-sections", "scripts/e2e-sidebar-sections.mjs", {}],
    ["right-sidebar-collapse", "scripts/e2e-right-sidebar-collapse.mjs", {}],
    ["mobile-drawers", "scripts/e2e-mobile-drawers.mjs", { TINE_E2E_FORCE_MOBILE_DRAWERS: "1" }],
    ["tab-overflow", "scripts/e2e-tab-overflow.mjs", {}],
    ["outline-guide", "scripts/e2e-outline-guide.mjs", {}],
    ["query-workspace", "scripts/e2e-query-workspace.mjs", {}],
    ["query-sheet", "scripts/e2e-query-sheet.mjs", {}],
    ["query-vocabulary", "scripts/e2e-query-vocabulary.mjs", {}],
    ["query-display", "scripts/e2e-query-display.mjs", {}],
    ["empty-query-workspace", "scripts/e2e-empty-query-workspace.mjs", {}],
    ["scrollbars", "scripts/e2e-scrollbars.mjs", {}],
    ["page-trailing-block", "scripts/e2e-page-trailing-block.mjs", {}],
    // Catalogued since 2026-08-09 and selected by nothing until 2026-08-17: seven
    // journeys, six of them `stability: "stable"`, including the published-site
    // security boundary. The catalog checker now enforces both directions, so a
    // journey can no longer sit in the contract without a runner.
    ["publish-security", "scripts/e2e-publish-security.mjs", {}],
    // Publish a query (static site + the read-only app over a baked snapshot).
    // e2e-published-app spawns e2e-publish-query as its producer, but the
    // catalog checker follows imports, not children, and the export journey
    // has its own contract — so both are selected here.
    ["publish-query", "scripts/e2e-publish-query.mjs", {}],
    ["published-app", "scripts/e2e-published-app.mjs", {}],
    ["page-identity-links", "scripts/e2e-page-identity-links.mjs", {}],
    ["external-graph-wide-changes", "scripts/e2e-external-graph-wide-changes.mjs", {}],
    ["concord-focus-freshness", "scripts/e2e-concord-focus-freshness.mjs", {}],
    ["concord-live-save", "scripts/e2e-concord-live-save.mjs", {}],
    ["concord-missing-target", "scripts/e2e-concord-live-save.mjs", { TINE_E2E_MISSING_TARGET: "1" }],
    ["concord-sync-copy-native", "scripts/e2e-concord-sync-copy.mjs", { TINE_E2E_WATCH_MODE: "inotify" }],
    ["concord-sync-copy-poll", "scripts/e2e-concord-sync-copy.mjs", { TINE_E2E_WATCH_MODE: "poll" }],
    ["concord-sync-copy-native-same-content", "scripts/e2e-concord-sync-copy.mjs", { TINE_E2E_WATCH_MODE: "inotify", TINE_E2E_CONCORD_DECISION: "mine" }],
    ["concord-sync-copy-poll-same-content", "scripts/e2e-concord-sync-copy.mjs", { TINE_E2E_WATCH_MODE: "poll", TINE_E2E_CONCORD_DECISION: "mine" }],
    ["rendered-delete-verify", "scripts/e2e-rendered-delete-verify.mjs", {}],
    ["delete-selection-timing", "scripts/e2e-delete-selection-timing.mjs", {}],
    ["selection-actions", "scripts/e2e-selection-actions.mjs", {}],
    ["clipboard-roundtrip", "scripts/e2e-clipboard-roundtrip.mjs", {}],
  ],
  "windows-smoke": [
    // og-parity-references is a HARD gate on Linux (linux-release + og-parity-pilot
    // suites), where adaptive page completion is byte-exact with OG. It is dropped
    // from the advisory Windows suite only: on WebView2 the completion yields a
    // trailing space and two Terra debugging rounds (2026-07-20) could not even
    // observe the reference-completion setting state to classify it as timing vs a
    // real WebView2 editor difference. Per AGENTS.md §2b E2E stop-loss (two dispatches,
    // still red + unclassified → quarantine + record debt), it is quarantined from
    // Windows so a future Windows regression is not masked by a chronic red line. The
    // open question (is the trailing space a genuine WebView2 product diff?) is tracked
    // in specs/notes; the assertion stays live everywhere it currently passes.
    ["page-properties", "scripts/e2e-page-properties.mjs", {}],
    ["pdf-logseq", "scripts/e2e-pdf-logseq.mjs", { E2E_WINDOW_MANAGER: "openbox" }],
    ["print-security", "scripts/e2e-print-security.mjs", {}],
    // selection-actions (GH #240) is QUARANTINED from Windows, 2026-09-05, with
    // the §2b E2E stop-loss exhausted after three dispatches. It stays a live,
    // stable, blocking journey on Linux; only Windows membership is withdrawn,
    // because e2e-contracts.json carries one stability per journey and
    // quarantining there would disable the only #240 evidence we have.
    //
    // What the three runs bought, so the next attempt does not re-buy it:
    //   33959023127 - EdgeDriver launch mode, "DevToolsActivePort file doesn't
    //     exist". Fixed in 94ffcfec: the journey now starts the app through
    //     startWebdriverApplication and attaches.
    //   33960379224 - ECONNREFUSED after 4.5s; msedgedriver had not bound its
    //     port and webdriverio does not retry a refused connection. Fixed in
    //     cb5307e2 by the same 2500ms wait e2e-page-properties.mjs uses.
    //   33961433495 - reached 20.0s, exactly openSession's 20s wait for
    //     ".page-title, .ls-block". The app is FINE: its tine-debug.log shows
    //     Direct Files publish succeeding at 3683ms on the seeded graph. So the
    //     remaining defect is between a healthy WebView2 attach and the first
    //     DOM query - a wrong DevTools target, or a first-run surface Linux
    //     does not show. That is where attempt four should start.
    //
    // Harness debt: the journey writes its rich failure capsule (step, expected,
    // observed roots, screenshot, webview errors) into its own TMP, which on
    // Windows is under the runner profile and is NOT uploaded - the workflow only
    // collects test-results/. Three runs produced no journey-level diagnostics.
    // Fixing that is a precondition for attempt four being cheaper than these.
    ["windows-core", "scripts/e2e-windows-smoke.mjs", {}],
    ["windows-direct-large-open", "scripts/e2e-windows-direct-large-open.mjs", {}],
    ["page-trailing-block", "scripts/e2e-page-trailing-block.mjs", {}],
    ["tab-overflow", "scripts/e2e-tab-overflow.mjs", {}],
  ],
};

if (!suites[suiteName]) {
  console.error(`unknown suite ${suiteName}; choose ${Object.keys(suites).join(", ")}`);
  process.exit(2);
}

function loadSelectedContracts(scenarios) {
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(contractsPath, "utf8"));
  } catch (error) {
    throw new Error(`could not load E2E contracts: ${error.message}`);
  }
  if (manifest.schemaVersion !== 1 || !manifest.scenarios || Array.isArray(manifest.scenarios) || typeof manifest.scenarios !== "object") {
    throw new Error("E2E contracts must have schemaVersion 1 and a scenarios object");
  }
  const selected = new Map();
  for (const [id, script] of scenarios) {
    const entry = manifest.scenarios[script];
    if (!entry || !Array.isArray(entry.contracts) || entry.contracts.length === 0) {
      throw new Error(`E2E contract missing for selected scenario ${id} (${script})`);
    }
    if (!Array.isArray(entry.acceptableVariations) || !Array.isArray(entry.nonRequirements) || !allowedStabilities.has(entry.stability)) {
      throw new Error(`E2E contract has invalid scenario fields for ${script}`);
    }
    for (const contract of entry.contracts) {
      if (!contract || !allowedContractClasses.has(contract.class) || typeof contract.userOutcome !== "string" || typeof contract.blocking !== "boolean") {
        throw new Error(`E2E contract has invalid contract fields for ${script}`);
      }
      if (contract.class === "exact-safety-interoperability" && !(typeof contract.authority === "string" && contract.authority.trim())) {
        throw new Error(`E2E exact-safety contract lacks authority for ${script}`);
      }
      if (contract.class === "flexible-presentation-heuristic" && contract.blocking) {
        throw new Error(`E2E flexible-presentation contract cannot block for ${script}`);
      }
    }
    if (entry.stability === "quarantined" && !(typeof entry.quarantineReason === "string" && entry.quarantineReason.trim())) {
      throw new Error(`E2E quarantined contract lacks a reason for ${script}`);
    }
    selected.set(script, entry);
  }
  return selected;
}

function validateEmbeddedFrontend() {
  const index = path.join(root, "dist/index.html");
  if (!fs.existsSync(index)) throw new Error("dist/index.html is missing; run scripts/deploy.sh to build the production frontend");
  const asset = fs.readFileSync(index, "utf8").match(/[A-Za-z0-9_]+-[A-Za-z0-9_-]+\.(?:js|css)/)?.[0];
  if (!asset) throw new Error("could not identify a hashed frontend asset in dist/index.html; run scripts/deploy.sh");
  if (!fs.readFileSync(app).includes(Buffer.from(asset))) {
    throw new Error(`binary does not embed current production frontend ${asset}; run scripts/deploy.sh.`);
  }
  return asset;
}

function sha256(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

let activeHarnessDelta = null;

function harnessDeltaOnly(receipt) {
  if (e2eMode === "release") {
    throw new Error("--allow-harness-delta is refused in release mode: a release candidate is proved on an exact, committed tree.");
  }
  if (promotionPlanPath) {
    throw new Error("--allow-harness-delta is refused with a promotion plan: promotion proves an exact, committed tree.");
  }
  const delta = receiptDelta(receipt.sourceRevision);
  const product = delta.filter((relative) => !HARNESS_PATH.test(relative));
  if (product.length) {
    throw receiptRemediation(
      `--allow-harness-delta cannot cover ${product.length} non-harness path(s): ${product.slice(0, 12).join(", ")}`
      + `${product.length > 12 ? ", …" : ""} — these can change the product, so the binary must be rebuilt`,
    );
  }
  if (!delta.length) return false;
  activeHarnessDelta = delta;
  return true;
}

function receiptRemediation(detail) {
  return new Error(`${detail}. Run scripts/deploy.sh or the build receipt helper for the exact app binary before running E2E.`);
}

function loadBuildReceipt() {
  let receipt;
  try {
    receipt = JSON.parse(fs.readFileSync(receiptPath, "utf8"));
  } catch (error) {
    throw receiptRemediation(`could not read build receipt ${receiptPath}: ${error.message}`);
  }
  if (!receipt || Array.isArray(receipt) || typeof receipt !== "object") {
    throw receiptRemediation(`build receipt ${receiptPath} must be a JSON object`);
  }
  return receipt;
}

function strictBase64(value) {
  if (typeof value !== "string") return undefined;
  const bytes = Buffer.from(value, "base64");
  return bytes.toString("base64") === value ? bytes : undefined;
}

/**
 * Paths that are observers, not product.
 *
 * A journey script and a test fixture are compiled into nothing: the binary
 * under test is byte-identical whether or not they changed. The build receipt
 * cannot see that distinction — it digests the whole working tree — so during
 * v0.6.984 every failed hypothesis about a journey had to be COMMITTED before
 * it could be run, and during a release each such commit produced a new
 * candidate SHA. This is what `--allow-harness-delta` is allowed to tolerate;
 * everything else still refuses, because everything else can change the
 * product.
 */
// Deliberately the same rule `tine-coordination`'s `is_non_product_path` uses
// for `integrate --test-only`, so "this cannot change the product" means one
// thing in the repository rather than two. Keep them in step: scripts/**, any
// tests/ directory, docs/** (see src/docsAreNotProduct.guard.test.ts for why
// that is sound in Rust), and any *.test.* file.
const HARNESS_PATH = /^(scripts\/|docs\/|.*\/tests\/|tests\/)|(^|\/)[^/]+\.test\.[a-z]+$/;

/**
 * Every path that could differ from the one the receipt was built from.
 *
 * Mirrors `build-e2e-inputs.mjs`'s pathspecs exactly: docs and the generated
 * Tauri schemas are not build inputs, so they cannot be part of a delta the
 * receipt would have noticed. Reads git's RAW output — `gitOutput` trims, which
 * eats the leading space of a ` M path` status line and shifts every path by
 * one character.
 */
function receiptDelta(receiptRevision) {
  const pathspecs = ["--", ".", ":(exclude)docs/**", ":(exclude)src-tauri/gen/schemas/**"];
  const status = spawnSync("git", ["status", "--porcelain=v1", "-z", "--untracked-files=all", ...pathspecs],
    { cwd: root, encoding: "utf8" });
  if (status.status !== 0) throw status.error || new Error(`git status failed: ${String(status.stderr || "").trim()}`);
  // In `-z` output a rename/copy is TWO records: "R  <new>" then a bare
  // "<old>". Consume the second explicitly rather than slicing three
  // characters off a path that has no status prefix.
  const records = status.stdout.split("\0").filter(Boolean);
  const worktree = [];
  for (let index = 0; index < records.length; index += 1) {
    const record = records[index];
    worktree.push(record.slice(3));
    if (/^[RC]/.test(record)) worktree.push(records[index += 1]);
  }
  const committed = receiptRevision === checkoutRevision
    ? []
    : gitOutput(["diff", "--name-only", receiptRevision, checkoutRevision, ...pathspecs]).split(/\r?\n/).filter(Boolean);
  return [...new Set([...worktree, ...committed])].sort();
}

function validateBuildReceiptInputs() {
  const receipt = loadBuildReceipt();
  const schemaProblems = [];
  let normalizedTauriManifest;
  if (![1, 2].includes(receipt.schemaVersion)) schemaProblems.push("schemaVersion must be 1 or 2");
  if (typeof receipt.sourceRevision !== "string" || !receipt.sourceRevision) schemaProblems.push("sourceRevision must be a non-empty string");
  if (typeof receipt.builtAt !== "string" || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(receipt.builtAt) || Number.isNaN(Date.parse(receipt.builtAt))) {
    schemaProblems.push("builtAt must be an ISO timestamp");
  }
  if (typeof receipt.frontendAsset !== "string" || !receipt.frontendAsset) schemaProblems.push("frontendAsset must be a non-empty string");
  if (!/^[a-f0-9]{64}$/i.test(receipt.appSha256 || "")) schemaProblems.push("appSha256 must be a SHA-256 hex digest");
  if (!/^[a-f0-9]{64}$/i.test(receipt.buildInputDigest || "")) schemaProblems.push("buildInputDigest must be a SHA-256 hex digest");
  if (receipt.schemaVersion === 2 && !/^[a-f0-9]{64}$/i.test(receipt.productInputDigest || "")) {
    schemaProblems.push("schemaVersion 2 productInputDigest must be a SHA-256 hex digest");
  }
  if (typeof receipt.buildInputsDirty !== "boolean") schemaProblems.push("buildInputsDirty must be a boolean");
  if (!Array.isArray(receipt.buildInputChanges) || !receipt.buildInputChanges.every((change) => typeof change === "string")) {
    schemaProblems.push("buildInputChanges must be an array of strings");
  }
  if (typeof receipt.buildInputsDirty === "boolean" && Array.isArray(receipt.buildInputChanges) && receipt.buildInputsDirty !== (receipt.buildInputChanges.length > 0)) {
    schemaProblems.push("buildInputsDirty must agree with buildInputChanges");
  }
  if (receipt.tauriManifestNormalization !== undefined) {
    const normalization = receipt.tauriManifestNormalization;
    normalizedTauriManifest = strictBase64(normalization?.normalizedContentBase64);
    if (!normalization || Array.isArray(normalization) || normalization.path !== "src-tauri/Cargo.toml"
      || !/^[a-f0-9]{64}$/i.test(normalization.normalizedSha256 || "") || !normalizedTauriManifest
      || crypto.createHash("sha256").update(normalizedTauriManifest || Buffer.alloc(0)).digest("hex") !== normalization.normalizedSha256) {
      schemaProblems.push("tauriManifestNormalization must contain the exact normalized src-tauri/Cargo.toml bytes");
    }
  }
  if (schemaProblems.length) {
    throw receiptRemediation(`invalid build receipt ${receiptPath}: ${schemaProblems.join(", ")}`);
  }
  if (promotionPlanPath) {
    let plan;
    try {
      plan = assertPromotionPlan(JSON.parse(fs.readFileSync(promotionPlanPath, "utf8")));
      validatePromotionPlanForCheckout(root, plan, receipt);
    } catch (error) {
      throw receiptRemediation(`promotion plan ${promotionPlanPath} does not authorize this binary/proof checkout (${error.message})`);
    }
    activePromotionPlan = plan;
  } else if (allowHarnessDelta && harnessDeltaOnly(receipt)) {
    // Deliberately skips the digest, revision and dirty checks below: every
    // differing path is an observer, so the binary still IS this source's
    // product. The summary and every failure capsule record which files
    // differed, so such a run cannot be mistaken for candidate evidence.
    console.log(`HARNESS DELTA (not candidate evidence): ${activeHarnessDelta.join(", ")}`);
  } else {
    let checkoutState;
    try {
      checkoutState = normalizedTauriManifest
        ? normalizedBuildInputState(root, normalizedTauriManifest)
        : buildInputState(root);
    } catch (error) {
      throw receiptRemediation(`build receipt ${receiptPath} was built from different build inputs than the current checkout (${error.message})`);
    }
    if (receipt.buildInputDigest !== checkoutState.digest) {
      // Name the stray/changed working-tree entries — a download into the worktree
      // (untracked, non-ignored) is the usual culprit, and the bare message hid it.
      const stray = checkoutState.changes?.length
        ? ` Working-tree entries not in the built inputs: ${checkoutState.changes.slice(0, 12).join(", ")}${checkoutState.changes.length > 12 ? ", …" : ""}.`
        : "";
      throw receiptRemediation(`build receipt ${receiptPath} was built from different build inputs than the current checkout.${stray}`);
    }
    if (receipt.sourceRevision !== checkoutRevision) {
      throw receiptRemediation(`build receipt ${receiptPath} was built from ${receipt.sourceRevision}, not checkout ${checkoutRevision}`);
    }
  }
  if (receipt.buildInputsDirty && !activeHarnessDelta) {
    // NOT `receiptRemediation`: "run deploy.sh" is the wrong remedy here and
    // sent v0.6.984 into a rebuild that could not possibly help. Dirtiness is
    // computed from the GIT WORKING TREE, so a rebuild recomputes the same
    // dirty flag; only committing (or reverting) the paths clears it.
    const changes = receipt.buildInputChanges.slice(0, 12).join(", ");
    throw new Error(
      `build receipt ${receiptPath} records dirty binary/frontend inputs: ${changes}`
      + `${receipt.buildInputChanges.length > 12 ? ", …" : ""}. Dirtiness is computed from the git working `
      + "tree, not from the binary, so rebuilding recomputes the same flag. Commit (or revert) those paths "
      + "and rebuild. If every one of them is a journey or test file, rerun with --allow-harness-delta — "
      + "which is refused for a release candidate.",
    );
  }
  return receipt;
}

function validateBuildReceiptArtifact(receipt, appSha256, frontendAsset) {
  if (receipt.appSha256 !== appSha256) {
    throw receiptRemediation(`build receipt ${receiptPath} hashes a different app binary`);
  }
  if (receipt.frontendAsset !== frontendAsset) {
    throw receiptRemediation(`build receipt ${receiptPath} names frontend asset ${receipt.frontendAsset}, but the current production frontend uses ${frontendAsset}`);
  }
  return {
    kind: activePromotionPlan ? "promoted-build-receipt" : "build-receipt",
    testedCommit: activePromotionPlan?.targetCommit ?? receipt.sourceRevision,
    receiptPath,
    sourceRevision: receipt.sourceRevision,
    binarySourceCommit: receipt.sourceRevision,
    builtAt: receipt.builtAt,
    frontendAsset: receipt.frontendAsset,
    appSha256: receipt.appSha256,
    buildInputDigest: receipt.buildInputDigest,
    productInputDigest: receipt.productInputDigest,
    buildInputsDirty: receipt.buildInputsDirty,
    buildInputChanges: receipt.buildInputChanges,
    ...(activeHarnessDelta ? { harnessDelta: activeHarnessDelta } : {}),
    ...(activePromotionPlan ? {
      promotion: {
        sourceRunId: activePromotionPlan.sourceRunId,
        sourceCommit: activePromotionPlan.sourceCommit,
        targetCommit: activePromotionPlan.targetCommit,
        productInputDigest: activePromotionPlan.productInputDigest,
        proofIdentity: activePromotionPlan.proofIdentity,
      },
    } : {}),
  };
}

function resolveBuildProvenanceInputs() {
  if (fs.existsSync(receiptPath)) return validateBuildReceiptInputs();
  throw receiptRemediation(`build receipt is required at ${receiptPath}`);
}

function failureIsBlocking(status, contractEntry, scenarioId) {
  if (status !== "failed") return false;
  // A quarantined native harness remains in the suite to retain its diagnostic
  // evidence, but cannot block either ordinary or release mode until it has a
  // deterministic semantic readiness predicate again.
  if (contractEntry.stability === "quarantined") return false;
  if (e2eMode === "release") {
    return contractEntry.contracts.some((contract) => contract.class !== "flexible-presentation-heuristic");
  }
  return contractEntry.contracts.some((contract) => contract.blocking);
}

function xmlEscape(value) {
  return String(value).replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll('"', "&quot;");
}

function hasRecordedSemanticFailure(output, errors) {
  // Scenarios may finish their assertions, then lose the driver during cleanup.
  // Positive failure evidence takes precedence over any later transport error;
  // successful checks alone do not rule out an infrastructure retry.
  const combined = `${output}\n${errors}`.replace(/\u001b\[[0-9;]*m/g, "");
  return /^\s*(?:FAIL:|[1-9]\d* FAILURES\b)/m.test(combined)
    || /\bAssertionError(?: \[[^\]]+\])?:/.test(combined)
    // A scenario's explicit expected/actual observation is an assertion even
    // when it uses Error rather than AssertionError. Anchor to the emitted
    // error line, not a source excerpt printed with an uncaught stack trace.
    || /^\s*Error: [^\r\n]*; expected=.+ actual=.+$/m.test(combined);
}

function isRetryableDriverTransportFailure(output, errors, timedOut) {
  if (timedOut || hasRecordedSemanticFailure(output, errors)) return false;
  const combined = `${output}\n${errors}`;
  const webDriverError = /WebDriverError/.test(combined);
  const invalidSession = /WebDriverError:\s*invalid session id\b/.test(combined);
  const transportLost = /\/session/.test(combined)
    && /(UND_ERR_SOCKET|ECONNREFUSED|ECONNRESET|socket hang up|DevToolsActivePort file doesn't exist)/.test(combined);
  return webDriverError && (invalidSession || transportLost);
}

function isRetryableNativeHarnessFailure(id, output, errors, timedOut) {
  if (timedOut || hasRecordedSemanticFailure(output, errors)) return false;
  const combined = `${output}\n${errors}`;
  // Page-properties proves the target editor and document focus before sending
  // ArrowDown, then records the capture-phase key event. Only a missing event is
  // transport infrastructure; a delivered-but-ignored key is a product failure.
  if (id === "page-properties") return /E2E_NATIVE_INPUT_UNDELIVERED page-properties ArrowDown/.test(combined);
  if (id === "pdf-logseq") return /E2E_NATIVE_CHOOSER_INPUT_UNDELIVERED/.test(combined);
  if (id !== "capture") return false;
  // Hosted Openbox occasionally leaves its active-window property pointing at
  // a frame destroyed during the short single-instance forwarder's teardown.
  // Retry the entire isolated scenario once; the second run must still prove
  // first-show native + DOM focus and save real keyboard input.
  const badWindow = /BadWindow \(invalid Window parameter\)/.test(combined);
  const xdotoolActiveWindowFailure = /xdo_get_active_window reported an error/.test(combined);
  const xGetActiveWindowFailure = /XGetWindowProperty\[_NET_ACTIVE_WINDOW\] failed/.test(combined);
  return (badWindow && (xdotoolActiveWindowFailure || xGetActiveWindowFailure))
    || (xdotoolActiveWindowFailure && xGetActiveWindowFailure);
}

function relativeArtifactPath(file) {
  return path.relative(root, file).split(path.sep).join("/") || ".";
}

function failureClassification(id, output, errors, timedOut) {
  if (isRetryableDriverTransportFailure(output, errors, timedOut) || isRetryableNativeHarnessFailure(id, output, errors, timedOut)) {
    return "infrastructure";
  }
  return "ambiguous";
}

function archiveInfrastructureAttempt(dir, attempt) {
  const archive = path.join(dir, `infrastructure-attempt-${attempt}`);
  fs.mkdirSync(archive, { recursive: true });
  for (const entry of fs.readdirSync(dir)) {
    if (entry.startsWith("infrastructure-attempt-")) continue;
    fs.renameSync(path.join(dir, entry), path.join(archive, entry));
  }
}

async function runScenario([id, script, extraEnv], contractEntry) {
  const started = Date.now();
  const dir = path.join(artifactRoot, id);
  fs.mkdirSync(dir, { recursive: true });
  for (let attempt = 1; attempt <= 2; attempt += 1) {
    const stdout = fs.openSync(path.join(dir, "stdout.log"), "w");
    const stderr = fs.openSync(path.join(dir, "stderr.log"), "w");
    const driverPort = await freeLoopbackPort();
    const nativePort = await freeLoopbackPort(new Set([driverPort]));
    const previewPort = await freeLoopbackPort(new Set([driverPort, nativePort]));
    const env = {
      ...baseProcessEnv,
      ...extraEnv,
      TINE_APP: app,
      E2E_ARTIFACT_DIR: dir,
      E2E_DRIVER_PORT: String(driverPort),
      E2E_NATIVE_PORT: String(nativePort),
      E2E_PREVIEW_PORT: String(previewPort),
      TINE_SOURCE_REVISION: buildProvenance.testedCommit || "",
      // Journeys that refuse to run against an unidentified artifact read the
      // exact receipted commit under this name; give them the same authority the
      // runner already proved rather than a hand-exported shell variable.
      TINE_CANDIDATE_COMMIT: buildProvenance.testedCommit || "",
      E2E_LEGACY_NOTES: "0",
      TAURI_DRIVER: process.env.TAURI_DRIVER || (process.platform === "win32" ? "msedgedriver.exe" : "tauri-driver"),
    };
    if (process.platform === "linux") {
      env.WEBKIT_DRIVER = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
    } else if (process.env.WEBKIT_DRIVER) {
      env.WEBKIT_DRIVER = process.env.WEBKIT_DRIVER;
    }
    // Windows WebView2 session creation can fail before WebDriver exposes any
    // application output. Preserve Tine's own startup milestones and panic hook
    // beside the scenario evidence so hosted failures can be classified as an
    // app regression or driver infrastructure rather than guessed from Edge's
    // generic DevToolsActivePort error.
    if (process.platform === "win32") {
      env.TINE_DEBUG = process.env.TINE_DEBUG || "1";
      env.TINE_DEBUG_LOG = process.env.TINE_DEBUG_LOG || path.join(dir, "tine-debug.log");
      env.RUST_BACKTRACE = process.env.RUST_BACKTRACE || "1";
      env.E2E_WEBVIEW_USER_DATA_ROOT = path.join(
        os.tmpdir(),
        `tine-e2e-webview2-${suiteName}-${id}-${process.pid}-${driverPort}`,
      );
    }
    if (id === "og-parity-references") {
      env.E2E_TMP_DIR = process.env.E2E_TMP_DIR
        || path.join(os.tmpdir(), `tine-e2e-${suiteName}-${id}-${process.pid}-${driverPort}`);
    }
    const nativeLinux = process.platform === "linux" && id !== "selection-wrap" && id !== "wayland-app-id";
    // Tauri's Linux single-instance plugin owns a well-known session-bus name.
    // Give each native scenario a private bus so a slow WebKit/Tauri teardown
    // cannot forward the next scenario into the previous app. Processes spawned
    // inside one scenario still share the bus, preserving the multigraph and
    // Quick Capture handoff coverage.
    const runner = nativeLinux ? "xvfb-run" : process.execPath;
    const runnerArgs = nativeLinux
      // Xvfb must wrap the private bus: D-Bus-activated GTK portal services need
      // DISPLAY in the activation environment for auxiliary-window behavior.
      ? ["-a", process.env.DBUS_RUN_SESSION || "dbus-run-session", "--", process.execPath, path.join(root, script)]
      : [path.join(root, script)];
    // `taskset` wraps the OUTERMOST process so the whole scenario tree — xvfb,
    // the bus, the driver and the app — inherits the reduced CPU set.
    const command = pinnedCpuList ? "taskset" : runner;
    const args = pinnedCpuList ? ["-c", pinnedCpuList, runner, ...runnerArgs] : runnerArgs;
    const child = spawn(command, args, { cwd: root, env, detached: process.platform !== "win32", stdio: ["ignore", stdout, stderr] });
    let timedOut = false;
    const scenarioTimeoutMs = Number(env.E2E_SCENARIO_TIMEOUT_MS || timeoutMs);
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        if (process.platform === "win32") child.kill("SIGKILL");
        else process.kill(-child.pid, "SIGKILL");
      } catch {}
    }, scenarioTimeoutMs);
    const result = await new Promise((resolve) => {
      child.once("error", (error) => resolve({ code: 1, error: String(error) }));
      child.once("exit", (code, signal) => resolve({ code: code ?? 1, signal }));
    });
    clearTimeout(timer);
    const leaked = process.platform === "win32" ? [] : await reapProcessGroup(child.pid);
    if (process.platform === "win32") {
      fs.writeFileSync(
        path.join(dir, "webview2-profile.json"),
        `${JSON.stringify(windowsWebviewProfileSnapshot(env.E2E_WEBVIEW_USER_DATA_ROOT), null, 2)}\n`,
      );
    }
    fs.closeSync(stdout);
    fs.closeSync(stderr);
    const output = fs.readFileSync(path.join(dir, "stdout.log"), "utf8");
    const errors = fs.readFileSync(path.join(dir, "stderr.log"), "utf8");
    let status = result.code === 0 && !timedOut ? "passed" : "failed";
    if (leaked.length > 0) {
      process.stdout.write(
        `LEAK ${id}: ${leaked.length} process(es) survived the scenario and were killed: `
        + `${leaked.map((entry) => `${entry.pid} ${entry.args}`).join(" | ")}\n`,
      );
      status = "failed";
    }
    const retryDriver = isRetryableDriverTransportFailure(output, errors, timedOut);
    const retryNativeHarness = isRetryableNativeHarnessFailure(id, output, errors, timedOut);
    if (status === "failed" && attempt === 1 && (retryDriver || retryNativeHarness)) {
      const reason = retryDriver
        ? "WebDriver session became invalid or lost transport"
        : id === "page-properties"
          ? "native ArrowDown did not reach the proven-ready WebView"
          : id === "pdf-logseq"
            ? "native input did not reach the mapped and active GTK file chooser"
          : "hosted X11 active-window state raced a destroyed transient frame";
      process.stdout.write(`RETRY ${id}: ${reason}; retaining attempt 1\n`);
      process.stdout.write(`${output.slice(-1200)}\n${errors.slice(-1200)}\n`);
      archiveInfrastructureAttempt(dir, attempt);
      continue;
    }
    const record = {
      id,
      script,
      status,
      buildProvenance,
      stability: contractEntry.stability,
      contracts: contractEntry.contracts,
      exitCode: result.code,
      signal: result.signal ?? null,
      timedOut,
      attempts: attempt,
      infrastructureRetries: attempt - 1,
      durationMs: Date.now() - started,
      leakedProcesses: leaked,
      blocking: failureIsBlocking(status, contractEntry, id),
    };
    if (status === "failed") {
      const failurePath = path.join(dir, "failure.json");
      // Measured only now, on the failure path, and never on a green run: a
      // journey that waited 10s for an external change and did not see it is
      // reporting the product only if the machine was answering. See
      // scripts/lib/e2e-machine-probe.mjs for what this cost us once.
      const machine = machineSnapshot(dir);
      record.failure = {
        machine,
        machineVerdict: describeMachine(machine),
        testedCommit: buildProvenance.testedCommit,
        buildProvenance,
        scenario: id,
        script,
        expectedOutcomes: contractEntry.contracts.map((contract) => contract.userOutcome),
        observation: { exitCode: record.exitCode, signal: record.signal, timedOut: record.timedOut },
        classification: failureClassification(id, output, errors, timedOut),
        artifacts: {
          scenario: relativeArtifactPath(dir),
          stdout: relativeArtifactPath(path.join(dir, "stdout.log")),
          stderr: relativeArtifactPath(path.join(dir, "stderr.log")),
          failureCapsule: relativeArtifactPath(failurePath),
        },
      };
      fs.writeFileSync(failurePath, `${JSON.stringify(record.failure, null, 2)}\n`);
    }
    fs.writeFileSync(path.join(dir, "result.json"), JSON.stringify(record, null, 2) + "\n");
    process.stdout.write(`${status === "passed" ? "PASS" : "FAIL"} ${id} (${(record.durationMs / 1000).toFixed(1)}s)\n`);
    if (status === "failed") {
      process.stdout.write(`MACHINE ${id}: ${record.failure.machineVerdict}\n`);
      process.stdout.write(`FAILURE CAPSULE ${JSON.stringify(record.failure)}\n`);
    }
    return record;
  }
  throw new Error(`unreachable scenario retry state for ${id}`);
}

let scenarios = suites[suiteName];
if (only) scenarios = scenarios.filter(([id]) => id === only);
if (!scenarios.length) throw new Error(`no scenario named ${only} in ${suiteName}`);

const receipt = resolveBuildProvenanceInputs();
if (!fs.existsSync(app)) {
  console.error(`production app binary not found: ${app}; build the exact E2E candidate and its receipt first.`);
  process.exit(2);
}
const frontendAsset = validateEmbeddedFrontend();
const appSha256 = sha256(app);
const buildProvenance = validateBuildReceiptArtifact(receipt, appSha256, frontendAsset);
if (options.has("--validate-build-receipt-only")) {
  console.log(`PASS build receipt ${receiptPath}`);
  process.exit(0);
}
const selectedContracts = loadSelectedContracts(scenarios);
fs.rmSync(artifactRoot, { recursive: true, force: true });
fs.mkdirSync(artifactRoot, { recursive: true });

const results = [];
startLoad();
try {
  for (const scenario of scenarios) results.push(await runScenario(scenario, selectedContracts.get(scenario[1])));
} finally {
  stopLoad();
}
const summary = {
  schemaVersion: 1,
  suite: suiteName,
  mode: e2eMode,
  app,
  appSha256,
  buildProvenance,
  platform: `${os.platform()}-${os.arch()}`,
  // A green run under load is stronger evidence than a green idle run, and a
  // run with a harness delta is weaker than either. Say which this was.
  load: { burners: underLoad, pinnedCpus: pinnedCpuList, cpuCount },
  startedAt: suiteStartedAt,
  passed: results.filter((result) => result.status === "passed").length,
  failed: results.filter((result) => result.status === "failed").length,
  blockingFailed: results.filter((result) => result.blocking).length,
  nonblockingFailed: results.filter((result) => result.status === "failed" && !result.blocking).length,
  quarantinedFailed: results.filter((result) => result.status === "failed" && result.stability === "quarantined").length,
  results,
};
fs.writeFileSync(path.join(artifactRoot, "summary.json"), JSON.stringify(summary, null, 2) + "\n");
const cases = results.map((result) =>
  result.status === "passed"
    ? `<testcase name="${xmlEscape(result.id)}" time="${result.durationMs / 1000}"/>`
    : result.blocking
      ? `<testcase name="${xmlEscape(result.id)}" time="${result.durationMs / 1000}"><failure message="${xmlEscape(`${result.failure.classification}: ${result.failure.expectedOutcomes.join("; ")}`)}">Classification: ${xmlEscape(result.failure.classification)}. Expected user outcome: ${xmlEscape(result.failure.expectedOutcomes.join("; "))}. See ${xmlEscape(result.failure.artifacts.stdout)} and ${xmlEscape(result.failure.artifacts.stderr)}</failure></testcase>`
      : `<testcase name="${xmlEscape(result.id)}" time="${result.durationMs / 1000}"><skipped message="nonblocking E2E observation">Classification: ${xmlEscape(result.failure.classification)}. Expected user outcome: ${xmlEscape(result.failure.expectedOutcomes.join("; "))}. See ${xmlEscape(result.failure.artifacts.stdout)} and ${xmlEscape(result.failure.artifacts.stderr)}</skipped></testcase>`
).join("");
fs.writeFileSync(path.join(artifactRoot, "junit.xml"), `<?xml version="1.0"?><testsuite name="${xmlEscape(suiteName)}" tests="${results.length}" failures="${summary.blockingFailed}" skipped="${summary.failed - summary.blockingFailed}">${cases}</testsuite>\n`);
console.log(`E2E ${suiteName} (${e2eMode}): ${summary.passed} passed, ${summary.blockingFailed} blocking failed, ${summary.nonblockingFailed} nonblocking failed (${summary.quarantinedFailed} quarantined); artifacts ${artifactRoot}`);
process.exit(summary.blockingFailed ? 1 : 0);
