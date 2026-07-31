import assert from "node:assert/strict";
import { lstat, mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, extname, join, relative } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

import { AnonymizeError, anonymizeGraph } from "./anonymize-graph.mjs";

const UUID_A = "123e4567-e89b-42d3-a456-426614174000";
const UUID_B = "550e8400-e29b-41d4-a716-446655440000";
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

async function fixture(t) {
  const container = await mkdtemp(join(tmpdir(), "tine-anonymize-test-"));
  const root = join(container, "graph");
  await mkdir(root);
  t.after(() => rm(container, { recursive: true, force: true }));
  return root;
}

async function writeGraph(root, files) {
  for (const [path, contents] of Object.entries(files)) {
    const target = join(root, path);
    await mkdir(join(target, ".."), { recursive: true });
    await writeFile(target, contents);
  }
}

async function listFiles(root) {
  const files = [];
  async function walk(directory) {
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      const target = join(directory, entry.name);
      if (entry.isDirectory()) await walk(target);
      else files.push({ path: relative(root, target), text: await readFile(target, "utf8") });
    }
  }
  await walk(root);
  return files;
}

function punctuationAndWhitespaceMask(text) {
  return [...text].map((ch) => /[\p{L}\p{M}\p{N}]/u.test(ch) ? "x" : ch).join("");
}

async function expectFailure(source, destination) {
  await assert.rejects(() => anonymizeGraph({ source, destination }), AnonymizeError);
}

test("recursively exports Markdown and Org files from nested and custom directories", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-nested");
  await writeGraph(source, {
    "pages/Client Alpha.md": "- [[Client Alpha]]\n",
    "pages/custom/nested/Research.markdown": "# Research\n",
    "journals/2026_07_31.org": "* TODO Plan\n",
    "arbitrary/人/deep/notes.org": "* private note\n",
    "notes.txt": "must not export\n",
  });

  const summary = await anonymizeGraph({ source, destination });
  const files = await listFiles(destination);
  const exported = files.filter((file) => file.path !== "anonymization-report.txt");

  assert.equal(summary.fileCount, 4);
  assert.deepEqual(exported.map((file) => extname(file.path).toLowerCase()).sort(), [".markdown", ".md", ".org", ".org"]);
  assert.equal(summary.formatCounts.md, 1);
  assert.equal(summary.formatCounts.markdown, 1);
  assert.equal(summary.formatCounts.org, 2);
  assert.ok(summary.maximumDirectoryDepth >= 3);
});

test("preserves UTF-8 byte length, whitespace, punctuation, and line endings", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-layout");
  const original = `\uFEFF- TODO [[Client Alpha]]  \r\n\t- id:: ${UUID_A}\r\n* DONE #Client_Alpha!\r\n`;
  await writeGraph(source, { "pages/Client Alpha.md": original });

  await anonymizeGraph({ source, destination });
  const output = (await listFiles(destination)).find((file) => extname(file.path).toLowerCase() === ".md").text;

  assert.equal(Buffer.byteLength(output), Buffer.byteLength(original));
  assert.equal(punctuationAndWhitespaceMask(output), punctuationAndWhitespaceMask(original));
  assert.deepEqual(output.match(/\r\n|\n|\r/g), original.match(/\r\n|\n|\r/g));
});

test("preserves repeated references and UUID identities while retaining parseable grammar", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-identities");
  await writeGraph(source, {
    "pages/Client Alpha.md": [
      "- TODO [[Client Alpha]] #ProjectX",
      "  alias:: Client Alpha",
      `  id:: ${UUID_A}`,
      `  reference ((` + UUID_A + "))",
      "- DONE [[Other Page]] #OtherTag",
      `  id:: ${UUID_B}`,
      "https://intranet.example/Client-Alpha",
      "#+TITLE: Client Alpha",
    ].join("\n"),
  });

  await anonymizeGraph({ source, destination });
  const output = (await listFiles(destination)).find((file) => extname(file.path).toLowerCase() === ".md").text;
  const references = [...output.matchAll(/\[\[([^\]]+)\]\]/g)].map((match) => match[1]);
  const ids = [...output.matchAll(/id:: ([0-9a-f-]{36})/gi)].map((match) => match[1]);
  const blockReference = output.match(/\(\(([0-9a-f-]{36})\)\)/i)?.[1];

  assert.equal(references[0], output.match(/alias:: (.+)/)?.[1]);
  assert.notEqual(references[0], "Client Alpha");
  assert.notEqual(references[0], references[1]);
  assert.equal(ids[0], blockReference);
  assert.notEqual(ids[0], ids[1]);
  assert.ok(ids.every((id) => UUID_PATTERN.test(id)));
  assert.match(output, /- TODO /);
  assert.match(output, /- DONE /);
  assert.match(output, /https:\/\//);
  assert.match(output, /#\+TITLE:/);
});

test("uses one pseudonym map for filename, directory, page, and link tokens", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-consistency");
  await writeGraph(source, {
    "pages/Private Clients/Secret Person.md": "- [[Secret Person]] met Private Clients\n",
  });

  await anonymizeGraph({ source, destination });
  const outputFile = (await listFiles(destination)).find((file) => extname(file.path).toLowerCase() === ".md");
  const pageName = outputFile.text.match(/\[\[([^\]]+)\]\]/)?.[1];
  const outputStem = basename(outputFile.path, extname(outputFile.path));
  const outputDirectory = outputFile.path.split("/").at(-2);

  assert.equal(outputStem, pageName);
  assert.ok(outputFile.text.includes(outputDirectory));
  assert.doesNotMatch(outputFile.path, /Secret|Private|Clients|Person/u);
});

test("does not retain fixture secrets in content, paths, report, or CLI stdout", async (t) => {
  const root = await fixture(t);
  const source = join(root, "source");
  const destination = join(root, "anonymized-output");
  await writeGraph(source, {
    "pages/Alice Example.md": "- Alice Example met Björk Þór, account 987654321.\n",
  });

  const run = spawnSync(process.execPath, ["scripts/anonymize-graph.mjs", "--source", source, "--destination", destination], {
    cwd: process.cwd(),
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr);
  const exported = await listFiles(destination);
  const combined = `${run.stdout}\n${exported.map((file) => `${file.path}\n${file.text}`).join("\n")}`;
  for (const secret of ["Alice", "Example", "Björk", "Þór", "987654321"]) assert.doesNotMatch(combined, new RegExp(secret, "u"));
  assert.doesNotMatch(run.stdout, /source/u);
});

test("fails closed for symlinks, invalid UTF-8, existing or nested destinations, and portable path collisions", async (t) => {
  const root = await fixture(t);

  const symlinkSource = join(root, "symlink-source");
  await writeGraph(symlinkSource, { "pages/real.md": "- real\n" });
  await symlink(join(symlinkSource, "pages", "real.md"), join(symlinkSource, "pages", "linked.md"));
  const symlinkDestination = join(root, "symlink-output");
  await expectFailure(symlinkSource, symlinkDestination);
  await assert.rejects(() => lstat(symlinkDestination));

  const utf8Source = join(root, "utf8-source");
  await writeGraph(utf8Source, { "pages/good.md": "- good\n" });
  await writeFile(join(utf8Source, "pages", "bad.md"), Buffer.from([0xff, 0xfe]));
  const utf8Destination = join(root, "utf8-output");
  await expectFailure(utf8Source, utf8Destination);
  await assert.rejects(() => lstat(utf8Destination));

  const existingSource = join(root, "existing-source");
  const existingDestination = join(root, "existing-output");
  await writeGraph(existingSource, { "pages/good.md": "- good\n" });
  await mkdir(existingDestination);
  await writeFile(join(existingDestination, "keep.txt"), "keep");
  await expectFailure(existingSource, existingDestination);
  assert.equal(await readFile(join(existingDestination, "keep.txt"), "utf8"), "keep");

  const nestedSource = join(root, "nested-source");
  await writeGraph(nestedSource, { "pages/good.md": "- good\n" });
  const nestedDestination = join(nestedSource, "output");
  await expectFailure(nestedSource, nestedDestination);
  await assert.rejects(() => lstat(nestedDestination));

  const collisionSource = join(root, "collision-source");
  await writeGraph(collisionSource, { "pages/Same.md": "- one\n", "pages/Same.MD": "- two\n" });
  const collisionDestination = join(root, "collision-output");
  await expectFailure(collisionSource, collisionDestination);
  await assert.rejects(() => lstat(collisionDestination));
});

test("omits internal and non-managed files", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-omissions");
  await writeGraph(source, {
    "pages/managed.md": "- included\n",
    "assets/secret.md": "private asset text\n",
    ".git/secret.md": "private git text\n",
    ".tine-sync/secret.org": "private sync text\n",
    "pages/secret.pdf": "not managed",
    "custom/not-managed.txt": "not managed",
  });

  const summary = await anonymizeGraph({ source, destination });
  const exported = await listFiles(destination);
  const combined = exported.map((file) => `${file.path}\n${file.text}`).join("\n");

  assert.equal(summary.fileCount, 1);
  assert.doesNotMatch(combined, /asset|git|sync|private|managed\.txt|\.pdf/u);
});

test("handles thousands of synthetic managed files with count and byte invariants", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-many-files");
  const fileCount = 2_048;
  const files = {};
  let totalBytes = 0;
  for (let index = 0; index < fileCount; index += 1) {
    const text = `- [[Performance Token ${String(index).padStart(4, "0")}]] #batch\n`;
    files[`pages/custom-${String(index % 32).padStart(2, "0")}/item-${String(index).padStart(4, "0")}.md`] = text;
    totalBytes += Buffer.byteLength(text);
  }
  await writeGraph(source, files);

  const summary = await anonymizeGraph({ source, destination });
  const exported = (await listFiles(destination)).filter((file) => file.path !== "anonymization-report.txt");

  assert.equal(summary.fileCount, fileCount);
  assert.equal(summary.totalBytes, totalBytes);
  assert.equal(exported.length, fileCount);
  assert.equal(exported.reduce((sum, file) => sum + Buffer.byteLength(file.text), 0), totalBytes);
});
