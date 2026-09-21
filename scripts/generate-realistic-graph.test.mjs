import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { generateRealisticGraph } from "./generate-realistic-graph.mjs";

const TODAY = new Date(2026, 8, 21);

async function graph(t, options = {}) {
  const root = await mkdtemp(join(tmpdir(), "realistic-graph-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const manifest = await generateRealisticGraph({ root, pages: 300, journals: 60, today: TODAY, ...options });
  return { root, manifest };
}

test("today's journal carries what renders at launch", async (t) => {
  const { root } = await graph(t);
  const today = await readFile(join(root, "journals", "2026_09_21.md"), "utf8");
  assert.match(today, /\[\[[^\]]+\]\]/, "a page link");
  assert.match(today, /(^|\s)#tag\d+/m, "a tag");
  assert.match(today, /\(\([0-9a-f-]{36}\)\)/, "a block reference");
  assert.match(today, /\{\{embed /, "an embed");
  assert.match(today, /\{\{query /, "a query");
  assert.match(today, /^\s*- (TODO|DONE|LATER|NOW|DOING|WAITING|CANCELED) /m, "a task");
  assert.match(today, /sentinel543/, "the search sentinel");
});

test("links and references resolve to pages and blocks that exist", async (t) => {
  const { root } = await graph(t);
  const pageFiles = await readdir(join(root, "pages"));
  const pageNames = new Set(pageFiles.map((file) => file.replace(/\.md$/, "").replaceAll("___", "/")));
  const texts = [];
  for (const dir of ["pages", "journals"]) {
    for (const file of await readdir(join(root, dir))) texts.push(await readFile(join(root, dir, file), "utf8"));
  }
  const all = texts.join("\n");
  const ids = new Set([...all.matchAll(/^\s*id:: ([0-9a-f-]{36})$/gm)].map((match) => match[1]));
  const links = [...all.matchAll(/\[\[([^\]]+)\]\]/g)].map((match) => match[1]);
  // Real graphs link a few hub pages far more than the rest.
  const counts = new Map();
  for (const name of links) counts.set(name, (counts.get(name) ?? 0) + 1);
  const top = Math.max(...counts.values());
  assert.ok(top >= 10 * (links.length / counts.size), `a hub page collects many links (top ${top})`);
  const refs = [...all.matchAll(/\(\(([0-9a-f-]{36})\)\)/g)].map((match) => match[1]);
  assert.ok(links.length > 0 && refs.length > 0);
  assert.deepEqual(links.filter((name) => !pageNames.has(name)), []);
  assert.deepEqual(refs.filter((id) => !ids.has(id)), []);
});

test("the same seed and date give the same graph", async (t) => {
  const first = await graph(t);
  const second = await graph(t);
  assert.deepEqual(first.manifest, second.manifest);
  const file = "journals/2026_09_21.md";
  assert.equal(await readFile(join(first.root, file), "utf8"), await readFile(join(second.root, file), "utf8"));
});
