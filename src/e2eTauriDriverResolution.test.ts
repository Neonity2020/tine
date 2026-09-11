// A journey that reconstructs `tauri-driver`'s location from where the repo or
// the machine sits on disk is broken in the only place batch work happens.
//
// Six journeys had grown five different answers to "where is tauri-driver".
// Two derived `<repo>/../.toolchain/cargo/bin/tauri-driver`, which resolves
// only when the checkout is a direct child of the workspace root; every agent
// worktree is one level deeper, so the path had never existed and the journey
// died on a bare `spawn ... ENOENT`. That reads like a missing toolchain, not a
// wrong guess, and it cost a debugging cycle during the P2 batch. Two others
// hardcoded one machine's absolute path, which cannot survive CI or any other
// host. `scripts/env.sh` already exports `CARGO_HOME` and puts the binary on
// `PATH`; nothing needs to know the repository's position on disk.
//
// `resolveTauriDriver` in `scripts/e2e-capabilities.mjs` is the blessed
// exemplar and the front door; `scripts/e2e-rendered-delete-verify.mjs` is a
// short call site to imitate.
//
// Two rules, deliberately different in strength:
//   - DEFECTS are pinned at ZERO. A path literal naming `.toolchain`, or an
//     absolute filesystem path to the driver, is always wrong.
//   - DUPLICATION is ratcheted. The surviving journey-local spellings read only
//     environment variables, so they are harmless-but-duplicated; the count may
//     fall but never rise, so the next journey imports the helper instead of
//     copying its nearest neighbour.
import { describe, expect, it } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptsDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "scripts");

// `e2e-capabilities.mjs` holds `resolveTauriDriver` itself, so it is the one
// file that legitimately names the paths; the library modules beside it host no
// journey. Everything else under `scripts/e2e-*.mjs` is a journey.
const NOT_JOURNEYS = new Set([
  "e2e-capabilities.mjs",
  "e2e-capabilities.test.mjs",
  "e2e-file-poll.mjs",
  "e2e-file-poll.test.mjs",
]);

const journeys = () =>
  fs
    .readdirSync(scriptsDir)
    .filter((name) => name.startsWith("e2e-") && name.endsWith(".mjs") && !NOT_JOURNEYS.has(name))
    .sort();

/** Prose about a rule is not an instance of breaking it. */
const isComment = (line: string) => /^\s*(?:\/\/|\/\*|\*)/.test(line);

/** A driver location built from the repo's or the machine's place on disk. */
export function hardcodedDriverPathSites(source: string): string[] {
  const sites: string[] = [];
  for (const line of source.split("\n")) {
    if (isComment(line) || !/tauri-driver/.test(line)) continue;
    if (/\.toolchain/.test(line)) sites.push(line.trim());
    else if (/["'`]\/(?:aux|home|Users)\//.test(line)) sites.push(line.trim());
  }
  return sites;
}

/** A journey answering "where is tauri-driver" for itself. */
export function localResolutionSites(source: string): string[] {
  return source
    .split("\n")
    .filter((line) => !isComment(line) && /\bTAURI_DRIVER\b/.test(line))
    .map((line) => line.trim());
}

// The count when the ratchet was written. Lowering it is always welcome:
// convert a journey to `resolveTauriDriver()` and drop the number.
const ALLOWED_LOCAL_RESOLUTIONS = 62;

describe("tauri-driver resolution", () => {
  it("never encodes where the repo or the machine sits on disk", () => {
    const offenders = journeys()
      .map((name) => [name, hardcodedDriverPathSites(fs.readFileSync(path.join(scriptsDir, name), "utf8"))] as const)
      .filter(([, sites]) => sites.length > 0);
    expect(
      offenders,
      "A tauri-driver path derived from the repository's location breaks in every agent worktree, "
        + "and an absolute machine path breaks on every other host. Call resolveTauriDriver() from "
        + "scripts/e2e-capabilities.mjs instead — see scripts/e2e-rendered-delete-verify.mjs.",
    ).toEqual([]);
  });

  it("does not grow new journey-local answers", () => {
    const total = journeys().reduce(
      (sum, name) => sum + localResolutionSites(fs.readFileSync(path.join(scriptsDir, name), "utf8")).length,
      0,
    );
    expect(
      total,
      "A new journey resolved tauri-driver itself. Import resolveTauriDriver from "
        + "scripts/e2e-capabilities.mjs — see scripts/e2e-rendered-delete-verify.mjs.",
    ).toBeLessThanOrEqual(ALLOWED_LOCAL_RESOLUTIONS);
  });

  it("flags the exact shapes that were removed", () => {
    // The literal pre-fix lines. If the scanner stops matching these it has
    // stopped being a guard, and the next copy of them lands unnoticed.
    const removed = [
      'const TD = process.env.TAURI_DRIVER || path.resolve(ROOT, "..", ".toolchain", "cargo", "bin", "tauri-driver");',
      'const TD = process.env.TAURI_DRIVER || "/aux/koutecky/logseq/.toolchain/cargo/bin/tauri-driver";',
      'const TD = process.env.TAURI_DRIVER || path.join(process.env.CARGO_HOME || "/aux/koutecky/logseq/.toolchain/cargo", "bin", "tauri-driver");',
      'const LOCAL_TAURI_DRIVER = path.resolve(ROOT, "..", ".toolchain", "cargo", "bin", "tauri-driver");',
    ];
    for (const line of removed) expect(hardcodedDriverPathSites(line), line).toHaveLength(1);
    // And the survivors, which read only environment variables, are not flagged.
    for (const line of [
      'const TD = process.env.TAURI_DRIVER || "tauri-driver";',
      'const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");',
    ]) expect(hardcodedDriverPathSites(line), line).toEqual([]);
  });

  it("resolves through TAURI_DRIVER, then CARGO_HOME, then PATH", async () => {
    // `scripts/e2e-capabilities.mjs` has no `.d.mts`, and writing a partial one
    // would hide its other fourteen exports from every future TypeScript
    // consumer. A non-literal specifier keeps this import untyped instead of
    // lying about the module's surface; the behaviour, not the types, is what
    // this assertion is for.
    const helper = "../scripts/e2e-capabilities.mjs";
    const { resolveTauriDriver } = await import(helper);
    expect(resolveTauriDriver({ TAURI_DRIVER: "/explicit/td", CARGO_HOME: "/c" }, () => true)).toBe("/explicit/td");
    expect(resolveTauriDriver({ CARGO_HOME: "/c" }, () => true)).toBe(path.join("/c", "bin", "tauri-driver"));
    expect(resolveTauriDriver({ CARGO_HOME: "/c" }, () => false)).toBe("tauri-driver");
    expect(resolveTauriDriver({}, () => false)).toBe("tauri-driver");
  });
});
