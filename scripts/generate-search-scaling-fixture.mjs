#!/usr/bin/env node

// Build the approved orthogonal search-scaling corpora from the existing
// dense synthetic graph:
//   original:    1,000 pages / 60,000 root blocks
//   blocksLarge: 1,000 pages / 600,000 root blocks, with no new name inputs
//   namesLarge: 10,000 pages / 60,000 root blocks, with 9,000 new titles
// The CLI deliberately accepts only that source and those dimensions; tests
// inject smaller dimensions through the exported function. Each output gets
// a manifest with pre/post source hashes, an output-content hash, and raw
// construction counts. Those checks are not a parsed graph census.

import { createHash } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { lstat, mkdir, readFile, readdir, realpath, writeFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { pipeline } from "node:stream/promises";

export const APPROVED_DIMENSIONS = Object.freeze({
  sourcePages: 1_000,
  sourceBlocksPerPage: 60,
  extraBlocksPerPage: 540,
  extraNamePages: 9_000,
});

const VARIANTS = new Set(["original", "blocksLarge", "namesLarge"]);
const MANIFEST_NAME = "search-scaling-fixture-manifest.json";

export class FixtureError extends Error {
  constructor(message) {
    super(message);
    this.name = "FixtureError";
  }
}

function compareNames(left, right) {
  return left < right ? -1 : left > right ? 1 : 0;
}

function sourcePagePath(index) {
  return `pages/主题-${String(index).padStart(5, "0")}.md`;
}

function sourcePageText(index, dimensions) {
  const next = (index + 1) % dimensions.sourcePages;
  let text = `title:: Topic ${index} 你好\n\n`;
  for (let block = 0; block < dimensions.sourceBlocksPerPage; block += 1) {
    text += `- outline sentinel543 你好世界 page ${index} block ${block} [[Topic ${next} 你好]] #tag${block % 10}\n`;
  }
  return text;
}

function extraBlocksText(page, dimensions) {
  let text = "";
  const first = dimensions.sourceBlocksPerPage;
  const end = first + dimensions.extraBlocksPerPage;
  for (let block = first; block < end; block += 1) {
    // This keeps the broad 你好 match while introducing no page reference,
    // tag, property, sparse needle, or false-positive needle.
    text += `- outline sentinel543 你好世界 page ${page} extra block ${block}\n`;
  }
  return text;
}

function validateDimensions(dimensions) {
  for (const [name, value] of Object.entries(dimensions)) {
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new FixtureError(`${name} must be a non-negative safe integer`);
    }
  }
  if (dimensions.sourcePages === 0 || dimensions.sourceBlocksPerPage === 0) {
    throw new FixtureError("source dimensions must contain pages and blocks");
  }
}

async function pathExists(path) {
  try {
    await lstat(path);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function listFiles(root) {
  const files = [];
  async function walk(directory, prefix) {
    const entries = await readdir(directory, { withFileTypes: true });
    entries.sort((left, right) => compareNames(left.name, right.name));
    for (const entry of entries) {
      const path = join(directory, entry.name);
      const child = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) await walk(path, child);
      else if (entry.isFile()) files.push(child);
      else throw new FixtureError(`synthetic source contains a non-regular entry: ${child}`);
    }
  }
  await walk(root, "");
  return files;
}

export async function hashFixtureTree(root, files = undefined) {
  const paths = files ?? await listFiles(root);
  const hash = createHash("sha256");
  for (const path of paths) {
    hash.update(`${Buffer.byteLength(path, "utf8")}:${path}\0`);
    for await (const chunk of createReadStream(join(root, path))) hash.update(chunk);
    hash.update("\0");
  }
  return hash.digest("hex");
}

async function preflightSource(source, dimensions) {
  const files = await listFiles(source);
  const expected = Array.from({ length: dimensions.sourcePages }, (_, index) => sourcePagePath(index));
  if (files.length !== expected.length || files.some((path, index) => path !== expected[index])) {
    throw new FixtureError(
      `source is not the bounded dense synthetic corpus: expected exactly ${expected.length} canonical page files`,
    );
  }

  for (let index = 0; index < dimensions.sourcePages; index += 1) {
    const path = expected[index];
    const actual = await readFile(join(source, path), "utf8");
    if (actual !== sourcePageText(index, dimensions)) {
      throw new FixtureError(`source page does not match the dense synthetic construction: ${path}`);
    }
  }
  return files;
}

function expectedCounts(variant, dimensions) {
  const sourceBlocks = dimensions.sourcePages * dimensions.sourceBlocksPerPage;
  if (variant === "blocksLarge") {
    return {
      rawFiles: dimensions.sourcePages,
      rawPageFiles: dimensions.sourcePages,
      rawRootBlocks: sourceBlocks + dimensions.sourcePages * dimensions.extraBlocksPerPage,
    };
  }
  if (variant === "namesLarge") {
    return {
      rawFiles: dimensions.sourcePages + dimensions.extraNamePages,
      rawPageFiles: dimensions.sourcePages + dimensions.extraNamePages,
      rawRootBlocks: sourceBlocks,
    };
  }
  return {
    rawFiles: dimensions.sourcePages,
    rawPageFiles: dimensions.sourcePages,
    rawRootBlocks: sourceBlocks,
  };
}

async function inspectRawOutput(output, files) {
  let rawRootBlocks = 0;
  let rawPageFiles = 0;
  for (const path of files) {
    if (!path.startsWith("pages/") || !path.endsWith(".md")) continue;
    rawPageFiles += 1;
    const text = await readFile(join(output, path), "utf8");
    rawRootBlocks += text.split("\n").filter((line) => line.startsWith("- ")).length;
  }
  return { rawFiles: files.length, rawPageFiles, rawRootBlocks };
}

function isInside(parent, child) {
  const path = relative(parent, child);
  return path !== "" && path !== ".." && !path.startsWith(`..${sep}`) && !isAbsolute(path);
}

async function copySourceFile(source, output, path) {
  const target = join(output, path);
  await mkdir(dirname(target), { recursive: true });
  await pipeline(createReadStream(join(source, path)), createWriteStream(target, { flags: "wx" }));
}

/**
 * Build one fixture. `dimensions` exists for small deterministic tests; the
 * command-line entry point always uses APPROVED_DIMENSIONS.
 */
export async function generateSearchScalingFixture({
  source,
  output,
  variant,
  dimensions = APPROVED_DIMENSIONS,
}) {
  if (!VARIANTS.has(variant)) {
    throw new FixtureError(`variant must be one of: ${[...VARIANTS].join(", ")}`);
  }
  validateDimensions(dimensions);

  const sourcePath = await realpath(resolve(source));
  const requestedOutputPath = resolve(output);
  const outputPath = join(await realpath(dirname(requestedOutputPath)), basename(requestedOutputPath));
  if (sourcePath === outputPath || isInside(sourcePath, outputPath)) {
    throw new FixtureError("output must not be the source or a directory inside it");
  }
  if (await pathExists(outputPath)) {
    throw new FixtureError(`output already exists; refusing to overwrite: ${outputPath}`);
  }

  const sourceHashBefore = await hashFixtureTree(sourcePath);
  const sourceFiles = await preflightSource(sourcePath, dimensions);

  // Non-recursive creation makes the absent-output promise race-safe.
  await mkdir(outputPath);
  for (let page = 0; page < dimensions.sourcePages; page += 1) {
    const path = sourcePagePath(page);
    await copySourceFile(sourcePath, outputPath, path);
    if (variant === "blocksLarge") {
      await writeFile(join(outputPath, path), extraBlocksText(page, dimensions), { flag: "a" });
    }
  }

  if (variant === "namesLarge") {
    for (let index = dimensions.sourcePages;
      index < dimensions.sourcePages + dimensions.extraNamePages;
      index += 1) {
      const name = `Topic ${index} 你好`;
      const path = join(outputPath, sourcePagePath(index));
      await writeFile(path, `title:: ${name}\n`, { flag: "wx" });
    }
  }

  const outputFiles = await listFiles(outputPath);
  const actualOutputCounts = await inspectRawOutput(outputPath, outputFiles);
  const outputCounts = expectedCounts(variant, dimensions);
  if (Object.keys(outputCounts).some((name) => actualOutputCounts[name] !== outputCounts[name])) {
    throw new FixtureError(
      `constructed raw counts ${JSON.stringify(actualOutputCounts)}, expected ${JSON.stringify(outputCounts)}`,
    );
  }
  const outputHash = await hashFixtureTree(outputPath, outputFiles);
  const sourceHashAfter = await hashFixtureTree(sourcePath);
  if (sourceHashAfter !== sourceHashBefore) {
    throw new FixtureError("source changed while the fixture was being generated");
  }

  const sourceCounts = {
    rawFiles: dimensions.sourcePages,
    rawPageFiles: dimensions.sourcePages,
    rawRootBlocks: dimensions.sourcePages * dimensions.sourceBlocksPerPage,
  };

  const manifest = {
    schema: 1,
    variant,
    source: {
      path: sourcePath,
      treeSha256Before: sourceHashBefore,
      treeSha256After: sourceHashAfter,
      counts: sourceCounts,
    },
    output: {
      treeSha256ExcludingManifest: outputHash,
      counts: outputCounts,
    },
    rawNameInputs: {
      sourceTitleValues: dimensions.sourcePages,
      sourceReferenceTargetValues: dimensions.sourcePages,
      sourceTagValues: Math.min(10, dimensions.sourceBlocksPerPage),
      addedPhysicalPageNames: variant === "namesLarge" ? dimensions.extraNamePages : 0,
      addedReferenceOrTagValues: 0,
    },
    construction: variant === "blocksLarge"
      ? `${dimensions.extraBlocksPerPage} ordinary broad-matching blocks appended per source page`
      : variant === "namesLarge"
        ? `${dimensions.extraNamePages} title-only physical pages added with zero root blocks`
        : "source graph files copied byte-for-byte",
    censusNote: "Counts are raw construction checks, not a parsed census; use the Rust probe for actual pages, blocks, and navigable names.",
    manifestExcludedFromRawCounts: true,
  };
  await writeFile(join(outputPath, MANIFEST_NAME), `${JSON.stringify(manifest, null, 2)}\n`, { flag: "wx" });
  return manifest;
}

function usage() {
  return [
    "Usage:",
    "  node scripts/generate-search-scaling-fixture.mjs --variant <original|blocksLarge|namesLarge> --source /path/to/dense60k --output <new-absent-directory>",
    "",
    "Source must exactly match the canonical 1,000-page/60,000-block synthetic corpus.",
    "",
    "Construction:",
    "  original copies the 1,000-page/60,000-block source graph files byte-for-byte.",
    "  blocksLarge appends 540 ordinary 你好 blocks per page (1,000 pages/600,000 blocks).",
    "  namesLarge adds title-only Topic 1000 你好..Topic 9999 你好 pages (10,000 pages/60,000 blocks).",
    "  The manifest records raw counts and source-before/source-after hashes; the Rust probe owns parsed counts.",
    "",
    "Example (do not run until the fixture change is reviewed):",
    "  node scripts/generate-search-scaling-fixture.mjs --variant blocksLarge --source /path/to/dense60k --output /tmp/tine-search-blocks-large-new",
  ].join("\n");
}

function parseArgs(args) {
  if (args.includes("--help")) return { help: true };
  const options = {};
  for (let index = 0; index < args.length; index += 2) {
    const flag = args[index];
    const value = args[index + 1];
    if (!["--source", "--output", "--variant"].includes(flag) || value === undefined) {
      throw new FixtureError(`invalid arguments\n${usage()}`);
    }
    options[flag.slice(2)] = value;
  }
  if (!options.source || !options.output || !options.variant) {
    throw new FixtureError(`--source, --output, and --variant are required\n${usage()}`);
  }
  return options;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    console.log(usage());
    return;
  }
  const manifest = await generateSearchScalingFixture({
    ...options,
    dimensions: APPROVED_DIMENSIONS,
  });
  console.log(JSON.stringify(manifest, null, 2));
}

if (fileURLToPath(import.meta.url) === resolve(process.argv[1] ?? "")) {
  main().catch((error) => {
    console.error(`${error.name}: ${error.message}`);
    process.exitCode = 1;
  });
}
