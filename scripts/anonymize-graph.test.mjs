import assert from "node:assert/strict";
import { link, lstat, mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
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

function unicodeWordCharacters(byteLength, count) {
  const characters = [];
  for (let codePoint = 0x80; codePoint <= 0x10ffff && characters.length < count; codePoint += 1) {
    if (codePoint >= 0xd800 && codePoint <= 0xdfff) continue;
    const character = String.fromCodePoint(codePoint);
    if (Buffer.byteLength(character) === byteLength
      && /^[\p{L}\p{M}\p{N}]$/u.test(character)
      && character.normalize("NFC") === character) characters.push(character);
  }
  assert.equal(characters.length, count);
  return characters;
}

function fixedWidthTokens(alphabet, width) {
  let tokens = [""];
  for (let index = 0; index < width; index += 1) {
    tokens = tokens.flatMap((prefix) => alphabet.map((character) => `${prefix}${character}`));
  }
  return tokens;
}

function assertDomainPermutation(original, transformed) {
  assert.equal(transformed.length, original.length);
  assert.equal(new Set(transformed).size, original.length);
  assert.deepEqual(transformed.map((token) => Buffer.byteLength(token)), original.map((token) => Buffer.byteLength(token)));
  transformed.forEach((token, index) => assert.notEqual(token, original[index]));
}

async function expectFailure(source, destination) {
  let failure;
  await assert.rejects(() => anonymizeGraph({ source, destination }), (error) => {
    failure = error;
    return error instanceof AnonymizeError;
  });
  return failure;
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

test("exports exhausted one-glyph domains by salted derangement without relaxing longer secrets", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "anonymized-glyph-domains");
  const digits = [..."0123456789"];
  const upper = [..."ABCDEFGHIJKLMNOPQRSTUVWXYZ"];
  const lower = [..."abcdefghijklmnopqrstuvwxyz"];
  const numericSecret = "98765432101234567890";
  const wordSecret = "ConfidentialIdentifier";
  await writeGraph(source, {
    "pages/domains.md": `${digits.join(" ")}\n${upper.join(" ")}\n${lower.join(" ")}\n${numericSecret} ${wordSecret}\n`,
  });

  await anonymizeGraph({ source, destination, salt: Buffer.alloc(32, 11) });
  const output = (await listFiles(destination)).find((file) => extname(file.path).toLowerCase() === ".md").text;
  const [outputDigits, outputUpper, outputLower, outputSecrets] = output.trim().split("\n").map((line) => line.split(" "));

  for (const [original, transformed] of [[digits, outputDigits], [upper, outputUpper], [lower, outputLower]]) {
    assert.equal(new Set(transformed).size, original.length);
    assert.deepEqual(transformed.map((token) => Buffer.byteLength(token)), original.map((token) => Buffer.byteLength(token)));
    transformed.forEach((token, index) => assert.notEqual(token, original[index]));
  }
  assert.equal(outputSecrets.length, 2);
  assert.notEqual(outputSecrets[0], numericSecret);
  assert.notEqual(outputSecrets[1], wordSecret);
});

test("supports realistic two-byte and three-byte single-character vocabularies deterministically", async (t) => {
  const root = await fixture(t);
  const source = join(root, "source");
  const firstDestination = join(root, "first-output");
  const secondDestination = join(root, "second-output");
  const twoByte = unicodeWordCharacters(2, 320);
  const threeByte = unicodeWordCharacters(3, 320);
  await writeGraph(source, { "pages/unicode.md": `${twoByte.join(" ")}\n${threeByte.join(" ")}\n` });

  const salt = Buffer.alloc(32, 18);
  await anonymizeGraph({ source, destination: firstDestination, salt });
  await anonymizeGraph({ source, destination: secondDestination, salt });
  const first = (await listFiles(firstDestination)).find((file) => extname(file.path).toLowerCase() === ".md").text;
  const second = (await listFiles(secondDestination)).find((file) => extname(file.path).toLowerCase() === ".md").text;
  const [outputTwoByte, outputThreeByte] = first.trim().split("\n").map((line) => line.split(" "));

  assert.equal(first, second);
  assertDomainPermutation(twoByte, outputTwoByte);
  assertDomainPermutation(threeByte, outputThreeByte);
});

test("exports saturated multi-character shape domains by deterministic derangement", async (t) => {
  const root = await fixture(t);
  const source = join(root, "source");
  const firstDestination = join(root, "first-output");
  const secondDestination = join(root, "second-output");
  const digits = fixedWidthTokens([..."0123456789"], 2);
  // "id" and "at" are protected parser grammar, so saturate the admissible two-letter
  // lowercase domain rather than requiring it as a generated pseudonym.
  const lower = fixedWidthTokens([..."abcdefghijklmnopqrstuvwxyz"], 2).filter((token) => !["at", "id"].includes(token));
  await writeGraph(source, { "pages/domains.md": `${digits.join(" ")}\n${lower.join(" ")}\n` });

  const salt = Buffer.alloc(32, 19);
  await anonymizeGraph({ source, destination: firstDestination, salt });
  await anonymizeGraph({ source, destination: secondDestination, salt });
  const first = (await listFiles(firstDestination)).find((file) => extname(file.path).toLowerCase() === ".md").text;
  const second = (await listFiles(secondDestination)).find((file) => extname(file.path).toLowerCase() === ".md").text;
  const [outputDigits, outputLower] = first.trim().split("\n").map((line) => line.split(" "));

  assert.equal(first, second);
  assertDomainPermutation(digits, outputDigits);
  assertDomainPermutation(lower, outputLower);
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

test("omits the full fixed, hidden-component, and provider policy before vocabulary mapping", async (t) => {
  const root = await fixture(t);
  const baselineSource = join(root, "baseline");
  const excludedSource = join(root, "excluded");
  const baselineDestination = join(root, "baseline-output");
  const excludedDestination = join(root, "excluded-output");
  const included = {
    "pages/managed.md": "- included stable 24680\n",
    "logseq/allowed.md": "- allowed stable text\n",
  };
  await writeGraph(baselineSource, included);
  await writeGraph(excludedSource, {
    ...included,
    "ASSETS/asset-secret.md": "private asset vocabulary 0\n",
    "Publish/publish-secret.org": "private publish vocabulary 1\n",
    ".TINE-SYNC/sync-secret.md": "private sync vocabulary 2\n",
    ".git/git-secret.md": "private git vocabulary 3\n",
    "deep/.hidden/hidden-secret.md": "private hidden vocabulary 4\n",
    "deep/Node_Modules/pkg/dependency-secret.md": "private dependency vocabulary 5\n",
    "Logseq/.RECYCLE/recycle-secret.md": "private recycle vocabulary 6\n",
    "LOGSEQ/BAK/backup-secret.md": "private backup vocabulary 7\n",
    "logseq/VERSION-FILES/version-secret.md": "private version vocabulary 8\n",
    "logseq/.TINE-TRASH/trash-secret.md": "private trash vocabulary 9\n",
    "pages/page.sync-conflict-20260731-120000-ABCDEF.md": "private provider vocabulary alpha\n",
    "pages/page (conflicted copy 2026-07-31).md": "private provider vocabulary beta\n",
    "pages/secret.pdf": "not managed",
    "custom/not-managed.txt": "not managed",
  });

  const salt = Buffer.alloc(32, 12);
  const baseline = await anonymizeGraph({ source: baselineSource, destination: baselineDestination, salt });
  const excluded = await anonymizeGraph({ source: excludedSource, destination: excludedDestination, salt });
  const baselineFiles = (await listFiles(baselineDestination)).filter((file) => file.path !== "anonymization-report.txt");
  const excludedFiles = (await listFiles(excludedDestination)).filter((file) => file.path !== "anonymization-report.txt");

  assert.equal(baseline.fileCount, 2);
  assert.equal(excluded.fileCount, 2);
  assert.deepEqual(excludedFiles, baselineFiles);
});

test("configured hidden prefixes are excluded before inventory and vocabulary mapping", async (t) => {
  const root = await fixture(t);
  const baselineSource = join(root, "baseline");
  const hiddenSource = join(root, "hidden");
  const baselineDestination = join(root, "baseline-output");
  const hiddenDestination = join(root, "hidden-output");
  const visible = {
    "pages/included.md": "- included stable 24680\n",
    "elsewhere/archive/page.org": "* visible elsewhere\n",
    "private/visible.md": "- invalid leading slash is inert\n",
  };
  await writeGraph(baselineSource, visible);
  await writeGraph(hiddenSource, {
    ...visible,
    "archive/secret.md": "private archive vocabulary alpha\n",
    "archive-old/secret.org": "private prefix vocabulary beta\n",
    "archive-note.md": "private file vocabulary gamma\n",
    "vault/private/secret.md": "private escaped vocabulary delta\n",
    "vault/private-old/secret.md": "private escaped prefix vocabulary epsilon\n",
    "logseq/config.edn": [
      String.raw`{:hidden ["archive/" "vault\u002fprivate/" "/private"`,
      "          #_ \"discarded\" 42 [:ignored] {:also \"ignored\"}",
      "          ; \"commented\"",
      "          ]}",
    ].join("\n"),
  });

  const salt = Buffer.alloc(32, 15);
  const baseline = await anonymizeGraph({ source: baselineSource, destination: baselineDestination, salt });
  const baselineFiles = (await listFiles(baselineDestination)).filter((file) => file.path !== "anonymization-report.txt");
  await writeGraph(hiddenSource, {
    "archive/generated-pseudonym-witness.md": baselineFiles.map((file) => file.text).join("\n"),
  });
  const hidden = await anonymizeGraph({ source: hiddenSource, destination: hiddenDestination, salt });
  const hiddenFiles = (await listFiles(hiddenDestination)).filter((file) => file.path !== "anonymization-report.txt");

  assert.equal(baseline.fileCount, 3);
  assert.equal(hidden.fileCount, 3);
  assert.deepEqual(hiddenFiles, baselineFiles);
});

test("comments, strings, nested forms, and discards containing :hidden are inert", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "inert-hidden-output");
  await writeGraph(source, {
    "pages/included.md": "- included\n",
    "private/visible.md": "- still visible\n",
    "logseq/config.edn": [
      "{:description \"literal :hidden [\\\"private\\\"]\"",
      " ; :hidden [\"private\"]",
      " :nested {:hidden [\"private\"]}",
      " :discarded #_ {:hidden [\"private\"]} true",
      " :quoted-var #'example/value}",
    ].join("\n"),
  });

  const summary = await anonymizeGraph({ source, destination, salt: Buffer.alloc(32, 16) });
  assert.equal(summary.fileCount, 2);
  assert.equal((await listFiles(destination)).filter((file) => file.path !== "anonymization-report.txt").length, 2);
});

test("a large unrelated config value does not consume the hidden-policy byte budget", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "large-unrelated-config-output");
  await writeGraph(source, {
    "pages/included.md": "- included\n",
    "private/secret.md": "- secret\n",
    "logseq/config.edn": `{:description "${"x".repeat(70 * 1024)}" :hidden ["private"]}`,
  });

  const summary = await anonymizeGraph({ source, destination, salt: Buffer.alloc(32, 17) });
  assert.equal(summary.fileCount, 1);
});

test("malformed, deep, duplicate, empty, and oversized hidden config fails closed", async (t) => {
  const root = await fixture(t);
  const cases = [
    ["malformed", "{:hidden [\"private\"]"],
    ["deep", `{:hidden [${"[".repeat(70)}nil${"]".repeat(70)}]}`],
    ["duplicate", "{:hidden [\"private\"] :hidden [\"archive\"]}"],
    ["empty", "{:hidden [\"\"]}"],
    ["oversized", `{:hidden [\"${"x".repeat(64 * 1024)}\"]}`],
  ];

  for (const [label, config] of cases) {
    const source = join(root, `${label}-source`);
    const destination = join(root, `${label}-output`);
    await writeGraph(source, {
      "pages/included.md": "- included\n",
      "private/secret.md": "- secret\n",
      "logseq/config.edn": config,
    });
    const failure = await expectFailure(source, destination);
    assert.match(failure.message, /configuration|hidden/u, label);
    assert.doesNotMatch(failure.message, /private|secret|archive/u, label);
    await assert.rejects(() => lstat(destination), label);
  }
});

test("rejects portable-equivalent source identities without destination residue", async (t) => {
  const root = await fixture(t);
  const cases = [
    ["ascii", "pages/Foo.md", "pages/foo.md"],
    ["normalization", "pages/Café.md", "pages/Cafe\u0301.md"],
    ["full-fold", "pages/Straße.md", "pages/STRASSE.md"],
  ];
  for (const [label, left, right] of cases) {
    const source = join(root, `${label}-source`);
    const destination = join(root, `${label}-output`);
    await writeGraph(source, { [left]: "- left\n", [right]: "- right\n" });
    const failure = await expectFailure(source, destination);
    assert.match(failure.message, /portable identity/u);
    assert.doesNotMatch(failure.message, /Foo|Café|Cafe|Straße|STRASSE/u);
    await assert.rejects(() => lstat(destination));
  }
});

test("exports a unique non-ASCII nested path with path/content identity intact", async (t) => {
  const source = await fixture(t);
  const destination = join(source, "..", "unicode-output");
  await writeGraph(source, {
    "pages/客户/秘密.md": "- [[秘密]] belongs to 客户\n",
  });

  await anonymizeGraph({ source, destination, salt: Buffer.alloc(32, 13) });
  const output = (await listFiles(destination)).find((file) => extname(file.path).toLowerCase() === ".md");
  const outputStem = basename(output.path, extname(output.path));
  const outputDirectory = output.path.split("/").at(-2);
  assert.equal(output.text.match(/\[\[([^\]]+)\]\]/u)?.[1], outputStem);
  assert.ok(output.text.includes(outputDirectory));
  assert.doesNotMatch(`${output.path}\n${output.text}`, /客户|秘密/u);
});

test("rejects a hard-linked managed file without leaving output", async (t) => {
  const root = await fixture(t);
  const source = join(root, "hard-link-source");
  const destination = join(root, "hard-link-output");
  const outside = join(root, "outside-secret.md");
  await writeFile(outside, "- outside secret\n");
  await mkdir(join(source, "pages"), { recursive: true });
  await link(outside, join(source, "pages", "linked.md"));

  const failure = await expectFailure(source, destination);
  assert.match(failure.message, /hard links/u);
  assert.doesNotMatch(failure.message, /outside|secret|linked/u);
  await assert.rejects(() => lstat(destination));
});

test("protected-range lookup work grows linearly by deterministic comparison count", async (t) => {
  const root = await fixture(t);
  const run = async (lines) => {
    const source = join(root, `work-${lines}-source`);
    const destination = join(root, `work-${lines}-output`);
    await writeGraph(source, { "pages/work.md": "- TODO private\n".repeat(lines) });
    return anonymizeGraph({ source, destination, salt: Buffer.alloc(32, 14) });
  };

  const small = await run(256);
  const large = await run(512);
  assert.ok(small.protectedRangeComparisons > 0);
  assert.ok(large.protectedRangeComparisons > small.protectedRangeComparisons);
  assert.ok(large.protectedRangeComparisons <= small.protectedRangeComparisons * 2.05 + 8);
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
