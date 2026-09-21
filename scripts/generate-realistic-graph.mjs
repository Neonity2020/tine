#!/usr/bin/env node

// A synthetic Logseq graph shaped like a real one, for launch and indexing
// performance work (GH #543). The old 10k-page probe graph was 10,000
// identical 60-block pages with one link each and a one-line journal, so it
// never rendered a link, a tag, a block reference or an embed at launch and
// could not see the whole-graph work those trigger.
//
// Per-file sizes and feature counts are drawn from quantiles measured on an
// anonymized real graph (1,075 files; counts only, no content). Every text is
// generated, so the output is safe to use on public CI. The graph is split
// between journals and pages because a real 10k graph cannot be mostly
// journals (8,500 daily journals is 23 years). Today's journal and the recent
// ones always carry links, tags, a block reference, an embed, a query and
// tasks, because those are what render the moment the app opens.
//
// Deterministic for a given seed and date.

import { mkdir, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Quantiles [p50, p90, p99, max] per file, from the anonymized graph.
export const MEASURED = Object.freeze({
  pages: {
    blocks: [18, 80, 257, 645],
    depth: [1, 3, 5, 6],
    links: [0, 1, 10, 19],
    tags: [0, 1, 12, 16],
    blockRefs: [0, 0, 2, 9],
    embeds: [0, 0, 1, 5],
    queries: [0, 0, 3, 9],
    blockProps: [1, 41, 129, 273],
    tasks: [0, 6, 54, 74],
  },
  journals: {
    blocks: [5, 27, 56, 85],
    depth: [0, 2, 4, 5],
    links: [0, 1, 1, 6],
    tags: [0, 4, 14, 24],
    blockRefs: [0, 0, 0, 1],
    embeds: [0, 0, 0, 0],
    queries: [0, 0, 0, 0],
    blockProps: [0, 1, 3, 21],
    tasks: [2, 10, 21, 42],
  },
});

const WORDS = (
  "idea note plan draft review meeting lemma proof bound graph vertex edge " +
  "matrix solver budget travel garden recipe paper deadline summary outline " +
  "question answer result method example theorem sketch reading project task"
).split(" ");
const MARKERS = ["TODO", "DONE", "LATER", "NOW", "DOING", "WAITING", "CANCELED"];

function rng(seed) {
  let state = seed >>> 0 || 1;
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return (state >>> 0) / 4294967296;
  };
}

/** Sample a count whose quantiles match [p50, p90, p99, max]. */
function sample(random, [p50, p90, p99, max]) {
  const u = random();
  const lerp = (a, b, lo, hi) => Math.round(a + ((b - a) * (u - lo)) / (hi - lo));
  if (u < 0.5) return lerp(0, p50, 0, 0.5);
  if (u < 0.9) return lerp(p50, p90, 0.5, 0.9);
  if (u < 0.99) return lerp(p90, p99, 0.9, 0.99);
  return lerp(p99, max, 0.99, 1);
}

const pick = (random, items) => items[Math.floor(random() * items.length)];
const words = (random, n) => Array.from({ length: n }, () => pick(random, WORDS)).join(" ");
const uuid = (n) => `00000000-0000-4000-8000-${n.toString(16).padStart(12, "0")}`;
const dateStem = (d) =>
  `${d.getUTCFullYear()}_${String(d.getUTCMonth() + 1).padStart(2, "0")}_${String(d.getUTCDate()).padStart(2, "0")}`;

/**
 * Write the graph under `root` and return a manifest of what it contains.
 * `today` is the date whose journal the app opens at launch.
 */
export async function generateRealisticGraph({
  root,
  pages = 7_000,
  journals = 3_000,
  seed = 543,
  today = new Date(),
  sentinel = "sentinel543",
}) {
  const random = rng(seed);
  await mkdir(join(root, "pages"), { recursive: true });
  await mkdir(join(root, "journals"), { recursive: true });
  await mkdir(join(root, "logseq"), { recursive: true });
  await writeFile(join(root, "logseq", "config.edn"), "{}\n");

  // Page names: a share are namespaced (`area/topic`), as in real graphs.
  const names = Array.from({ length: pages }, (_, i) =>
    i % 12 === 0 ? `Area ${i % 40}/Topic ${i}` : `Topic ${i} ${pick(random, WORDS)}`,
  );
  const tags = Array.from({ length: 60 }, (_, i) => `tag${i}`);
  // Block ids some pages define and others reference or embed.
  const ids = [];
  let nextId = 1;
  const manifest = { pages, journals, blocks: 0, links: 0, tags: 0, blockRefs: 0, embeds: 0, queries: 0, sentinelBlocks: 0 };

  const block = (random, depth, features, owner) => {
    const indent = "  ".repeat(depth);
    let text = words(random, 3 + Math.floor(random() * 10));
    if (features.task > 0) {
      text = `${pick(random, MARKERS)} ${text}`;
      features.task--;
    }
    if (features.link > 0) {
      text += ` [[${pick(random, names)}]]`;
      features.link--;
      manifest.links++;
    }
    if (features.tag > 0) {
      text += ` #${pick(random, tags)}`;
      features.tag--;
      manifest.tags++;
    }
    if (features.ref > 0 && ids.length) {
      text += ` ((${pick(random, ids)}))`;
      features.ref--;
      manifest.blockRefs++;
    }
    let body = `${indent}- ${text}\n`;
    if (features.embed > 0 && ids.length) {
      body = `${indent}- {{embed ((${pick(random, ids)}))}}\n`;
      features.embed--;
      manifest.embeds++;
    } else if (features.query > 0) {
      body = `${indent}- {{query (task TODO DOING)}}\n`;
      features.query--;
      manifest.queries++;
    }
    if (features.prop > 0) {
      body += `${indent}  status:: ${pick(random, WORDS)}\n`;
      features.prop--;
    }
    if (features.id > 0) {
      const id = uuid(nextId++);
      ids.push(id);
      body += `${indent}  id:: ${id}\n`;
      features.id--;
    }
    if (owner.sentinel) {
      body += `${indent}- ${sentinel} ${words(random, 4)}\n`;
      owner.sentinel = false;
      manifest.sentinelBlocks++;
      manifest.blocks++;
    }
    manifest.blocks++;
    return body;
  };

  const outline = (random, shape, minimum, owner) => {
    const count = Math.max(1, sample(random, shape.blocks), minimum.blocks ?? 0);
    const maxDepth = sample(random, shape.depth);
    const features = {
      link: Math.max(sample(random, shape.links), minimum.links ?? 0),
      tag: Math.max(sample(random, shape.tags), minimum.tags ?? 0),
      ref: Math.max(sample(random, shape.blockRefs), minimum.blockRefs ?? 0),
      embed: Math.max(sample(random, shape.embeds), minimum.embeds ?? 0),
      query: Math.max(sample(random, shape.queries), minimum.queries ?? 0),
      prop: sample(random, shape.blockProps),
      task: Math.max(sample(random, shape.tasks), minimum.tasks ?? 0),
      id: owner.ids ?? 0,
    };
    let text = "";
    let depth = 0;
    for (let i = 0; i < count; i++) {
      text += block(random, depth, features, owner);
      depth = Math.min(maxDepth, Math.max(0, depth + (random() < 0.3 ? 1 : random() < 0.4 ? -1 : 0)));
    }
    return text;
  };

  for (let i = 0; i < pages; i++) {
    const name = names[i];
    let text = "";
    if (i % 50 === 0) text += `alias:: ${name} alias\n`;
    if (i % 40 === 0) text += `tags:: ${pick(random, tags)}, ${pick(random, tags)}\n`;
    if (i % 200 === 0) text += "template:: weekly\n";
    if (i % 25 === 0) text += "public:: true\n";
    if (text) text += "\n";
    // About one page in eight defines block ids that others reference.
    text += outline(random, MEASURED.pages, {}, { ids: i % 8 === 0 ? 1 + sample(random, [2, 8, 32, 68]) : 0, sentinel: i % 100 === 0 });
    const file = name.replaceAll("/", "___");
    await writeFile(join(root, "pages", `${file}.md`), text);
  }

  for (let i = 0; i < journals; i++) {
    const date = new Date(Date.UTC(today.getFullYear(), today.getMonth(), today.getDate() - i));
    // Today and the recent days are on screen at launch: guarantee the
    // render-time features there.
    const recent = i < 7;
    const minimum = recent
      ? { blocks: 12, links: 3, tags: 3, blockRefs: 1, embeds: i === 0 ? 1 : 0, queries: i === 0 ? 1 : 0, tasks: 3 }
      : {};
    let text = outline(random, MEASURED.journals, minimum, { sentinel: i === 0 });
    if (i < 60 && i % 5 === 0) text += `- TODO follow up on ${words(random, 3)}\n  SCHEDULED: <${dateStem(date).replaceAll("_", "-")}>\n`;
    await writeFile(join(root, "journals", `${dateStem(date)}.md`), text);
  }
  return manifest;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [root, pages = "7000", journals = "3000", seed = "543"] = process.argv.slice(2);
  if (!root) {
    console.error("usage: generate-realistic-graph.mjs ROOT [PAGES] [JOURNALS] [SEED]");
    process.exit(2);
  }
  const manifest = await generateRealisticGraph({
    root: resolve(root),
    pages: Number(pages),
    journals: Number(journals),
    seed: Number(seed),
  });
  console.log(JSON.stringify(manifest));
}
