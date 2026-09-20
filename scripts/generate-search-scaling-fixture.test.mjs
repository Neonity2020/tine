import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  FixtureError,
  generateSearchScalingFixture,
  hashFixtureTree,
} from "./generate-search-scaling-fixture.mjs";

const DIMENSIONS = Object.freeze({
  sourcePages: 3,
  sourceBlocksPerPage: 2,
  extraBlocksPerPage: 3,
  extraNamePages: 4,
});

const PREFIX_TRAP_DIMENSIONS = Object.freeze({
  sourcePages: 12,
  sourceBlocksPerPage: 60,
  extraBlocksPerPage: 60,
  extraNamePages: 0,
});

async function workspace(t) {
  const root = await mkdtemp(join(tmpdir(), "tine-search-scaling-test-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

function sourcePageText(page, dimensions = DIMENSIONS) {
  const next = (page + 1) % dimensions.sourcePages;
  let text = `title:: Topic ${page} 你好\n\n`;
  for (let block = 0; block < dimensions.sourceBlocksPerPage; block += 1) {
    text += `- outline sentinel543 你好世界 page ${page} block ${block} [[Topic ${next} 你好]] #tag${block % 10}\n`;
  }
  return text;
}

async function writeSource(root, dimensions = DIMENSIONS) {
  const source = join(root, "source");
  await mkdir(join(source, "pages"), { recursive: true });
  for (let page = 0; page < dimensions.sourcePages; page += 1) {
    const name = `主题-${String(page).padStart(5, "0")}.md`;
    await writeFile(join(source, "pages", name), sourcePageText(page, dimensions));
  }
  return source;
}

async function pageFiles(root) {
  return (await readdir(join(root, "pages"))).sort();
}

function rootBlockCount(text) {
  return text.split("\n").filter((line) => line.startsWith("- ")).length;
}

test("blocksLarge fixes pages and names while growing only ordinary broad-match blocks", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root);
  const output = join(root, "blocks-large");
  const sourceHash = await hashFixtureTree(source);

  const manifest = await generateSearchScalingFixture({
    source,
    output,
    variant: "blocksLarge",
    dimensions: DIMENSIONS,
  });

  assert.equal(await hashFixtureTree(source), sourceHash);
  assert.deepEqual(await pageFiles(output), await pageFiles(source));
  assert.deepEqual(manifest.source.counts, { rawFiles: 3, rawPageFiles: 3, rawRootBlocks: 6 });
  assert.deepEqual(manifest.output.counts, { rawFiles: 3, rawPageFiles: 3, rawRootBlocks: 15 });
  assert.equal(manifest.rawNameInputs.addedPhysicalPageNames, 0);
  assert.equal(manifest.rawNameInputs.addedReferenceOrTagValues, 0);

  for (let page = 0; page < DIMENSIONS.sourcePages; page += 1) {
    const name = `主题-${String(page).padStart(5, "0")}.md`;
    const text = await readFile(join(output, "pages", name), "utf8");
    const appended = text.slice(sourcePageText(page).length);
    assert.ok(text.startsWith(sourcePageText(page)));
    assert.equal(rootBlockCount(text), 5);
    assert.equal(rootBlockCount(appended), 3);
    assert.equal(appended, [2, 3, 4]
      .map((block) => `- outline sentinel543 你好世界 page ${page} extra block ${block}\n`)
      .join(""));
    assert.match(appended, /你好/);
    assert.doesNotMatch(appended, /\[\[|#tag|::|s7sparseanchor543|match/);
  }
});

test("blocksLarge extra block namespace cannot prefix-match an original exact needle", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root, PREFIX_TRAP_DIMENSIONS);
  const output = join(root, "blocks-large-prefix-trap");
  const original = sourcePageText(11, PREFIX_TRAP_DIMENSIONS);
  const falsePositiveNeedle = "page 11 block 11";
  const sparseNeedle = "s7sparseanchor543";

  await generateSearchScalingFixture({
    source,
    output,
    variant: "blocksLarge",
    dimensions: PREFIX_TRAP_DIMENSIONS,
  });

  const generated = await readFile(join(output, "pages", "主题-00011.md"), "utf8");
  const appended = generated.slice(original.length);
  assert.equal(generated.slice(0, original.length), original);
  assert.equal(original.split(falsePositiveNeedle).length - 1, 1);
  assert.equal(generated.split(falsePositiveNeedle).length - 1, 1);
  assert.doesNotMatch(appended, /page 11 block 11/);
  assert.doesNotMatch(appended, new RegExp(sparseNeedle));
  assert.equal(appended.match(/你好/g)?.length, PREFIX_TRAP_DIMENSIONS.extraBlocksPerPage);
  assert.equal(appended.split("\n").filter(Boolean).at(-1),
    "- outline sentinel543 你好世界 page 11 extra block 119");
  assert.doesNotMatch(appended, /\[\[|#tag|::/);
});

test("namesLarge preserves source blocks and adds only deterministic title-only physical pages", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root);
  const first = join(root, "names-first");
  const second = join(root, "names-second");

  const firstManifest = await generateSearchScalingFixture({
    source,
    output: first,
    variant: "namesLarge",
    dimensions: DIMENSIONS,
  });
  const secondManifest = await generateSearchScalingFixture({
    source,
    output: second,
    variant: "namesLarge",
    dimensions: DIMENSIONS,
  });

  assert.deepEqual(firstManifest.output.counts, { rawFiles: 7, rawPageFiles: 7, rawRootBlocks: 6 });
  assert.equal(firstManifest.rawNameInputs.addedPhysicalPageNames, 4);
  assert.deepEqual(firstManifest, secondManifest);
  assert.deepEqual(await pageFiles(first), await pageFiles(second));
  for (let index = 3; index < 7; index += 1) {
    const filename = `主题-${String(index).padStart(5, "0")}.md`;
    const text = await readFile(join(first, "pages", filename), "utf8");
    assert.equal(text, `title:: Topic ${index} 你好\n`);
    assert.equal(rootBlockCount(text), 0);
  }
});

test("original copies graph files byte-for-byte and writes a raw-count manifest", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root);
  const output = join(root, "original");

  const manifest = await generateSearchScalingFixture({
    source,
    output,
    variant: "original",
    dimensions: DIMENSIONS,
  });

  assert.equal(manifest.source.treeSha256Before, manifest.source.treeSha256After);
  assert.equal(manifest.source.treeSha256Before, manifest.output.treeSha256ExcludingManifest);
  assert.deepEqual(manifest.output.counts, { rawFiles: 3, rawPageFiles: 3, rawRootBlocks: 6 });
  assert.match(manifest.censusNote, /not a parsed census/);
  const recorded = JSON.parse(await readFile(join(output, "search-scaling-fixture-manifest.json"), "utf8"));
  assert.deepEqual(recorded, manifest);
});

test("refuses an existing output without changing it", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root);
  const output = join(root, "already-there");
  await mkdir(output);
  await writeFile(join(output, "sentinel"), "keep me\n");

  await assert.rejects(
    generateSearchScalingFixture({ source, output, variant: "blocksLarge", dimensions: DIMENSIONS }),
    (error) => error instanceof FixtureError && /refusing to overwrite/.test(error.message),
  );
  assert.equal(await readFile(join(output, "sentinel"), "utf8"), "keep me\n");
});

test("refuses a new output reached through a symlink into the source", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root);
  const sourceAlias = join(root, "source-alias");
  await symlink(source, sourceAlias, "dir");

  await assert.rejects(
    generateSearchScalingFixture({
      source,
      output: join(sourceAlias, "new-output"),
      variant: "original",
      dimensions: DIMENSIONS,
    }),
    (error) => error instanceof FixtureError && /directory inside it/.test(error.message),
  );
  await assert.rejects(readFile(join(source, "new-output", "search-scaling-fixture-manifest.json")), /ENOENT/);
});

test("rejects a source outside the bounded synthetic construction before creating output", async (t) => {
  const root = await workspace(t);
  const source = await writeSource(root);
  const output = join(root, "must-stay-absent");
  await writeFile(join(source, "pages", "主题-00001.md"), "- unrelated graph\n");

  await assert.rejects(
    generateSearchScalingFixture({ source, output, variant: "namesLarge", dimensions: DIMENSIONS }),
    (error) => error instanceof FixtureError && /dense synthetic construction/.test(error.message),
  );
  await assert.rejects(readFile(join(output, "search-scaling-fixture-manifest.json")), /ENOENT/);
});
