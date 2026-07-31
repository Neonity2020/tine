#!/usr/bin/env node
/**
 * Export a graph-shaped, content-anonymized fixture for local debugging.
 *
 * This is deliberately a standalone developer tool.  It has no network code,
 * never reads from the application store, and only writes a destination that
 * did not exist when the command started.
 */

import { createHmac, randomBytes } from "node:crypto";
import { lstat, mkdir, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { dirname, extname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
const encoder = new TextEncoder();
const MANAGED_EXTENSIONS = new Set([".md", ".markdown", ".org"]);
const OMITTED_DIRECTORIES = new Set([".git", ".tine-sync", "assets"]);
const STRUCTURAL_ROOT_DIRECTORIES = new Set(["pages", "journals"]);
const REPORT_NAME = "anonymization-report.txt";
const WORD_RE = /[\p{L}\p{M}\p{N}]+/gu;
const UUID_RE = /(?<![\p{L}\p{N}_])[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}(?![\p{L}\p{N}_])/giu;

// These are fixed parser grammar, not user identifiers.  In particular, a
// custom property or directive is *not* protected merely because it looks like
// one of these constructs.
const TASK_MARKERS = new Set(["todo", "doing", "done", "now", "later", "waiting", "wait", "canceled", "cancelled"]);
const STANDARD_PROPERTIES = new Set([
  "id", "collapsed", "background-color", "created-at", "updated-at", "title",
  "alias", "aliases", "tags", "filetags", "roam_tags", "roam_alias", "public",
  "template", "template-including-parent", "scheduled", "deadline", "closed",
  "marker", "priority", "logseq.order-list-type", "logseq.color",
  "logseq.table.version", "logseq.table.headers", "logseq.table.rows",
]);
const ORG_DIRECTIVES = new Set([
  "title", "alias", "tags", "filetags", "roam_tags", "property", "properties",
  "options", "startup", "author", "date", "name", "caption", "results",
  "attr_html", "attr_org",
]);
const URL_SCHEMES = new Set(["http", "https", "ftp", "ftps", "file", "mailto", "tel", "data", "irc", "ircs", "ssh", "git", "magnet"]);
const GENERATED_PUBLIC_WORDS = new Set([
  ...TASK_MARKERS,
  ...[...STANDARD_PROPERTIES].flatMap((name) => name.split(/[-.]/)),
  ...ORG_DIRECTIVES,
  ...URL_SCHEMES,
  "properties", "end", "begin", "scheduled", "deadline", "closed",
]);

const ALPHABETS = {
  upper: [..."ABCDEFGHIJKLMNOPQRSTUVWXYZ"],
  lower: [..."abcdefghijklmnopqrstuvwxyz"],
  digit: [..."0123456789"],
  two: [..."äöüßáéíóúñçøåæþðšž"],
  three: [..."中文日本語水火木金土月日天地人山川"],
  four: ["𐐀", "𐐁", "𐐂", "𐐃", "𐐄", "𐐅", "𐐆", "𐐇", "𐐈", "𐐉"],
};

export class AnonymizeError extends Error {
  constructor(message) {
    super(message);
    this.name = "AnonymizeError";
  }
}

function isManagedFile(name) {
  return MANAGED_EXTENSIONS.has(extname(name).toLowerCase());
}

function isContained(child, parent) {
  const value = relative(parent, child);
  return value === "" || (!value.startsWith("..") && !value.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`) && !value.includes("\0"));
}

function safeError(message) {
  throw new AnonymizeError(message);
}

function addMatchRange(ranges, match, capture = 0) {
  const matched = match[capture];
  if (!matched) return;
  const offset = capture === 0 ? 0 : match[0].indexOf(matched);
  ranges.push({ start: match.index + offset, end: match.index + offset + matched.length });
}

function grammarProtectedRanges(text) {
  const ranges = [];

  for (const match of text.matchAll(/(?:^|\r?\n)[\t ]*(?:[-+*]|\d+[.)])[\t ]+(?:\[[ Xx-]\][\t ]+)?(TODO|DOING|DONE|NOW|LATER|WAITING|WAIT|CANCELED|CANCELLED)(?=[\t ]|$)/gim)) {
    addMatchRange(ranges, match, 1);
  }
  for (const match of text.matchAll(/(?:^|\r?\n)[\t ]*\*+[\t ]+(TODO|DOING|DONE|NOW|LATER|WAITING|WAIT|CANCELED|CANCELLED)(?=[\t ]|$)/gim)) {
    addMatchRange(ranges, match, 1);
  }
  for (const match of text.matchAll(/\b(SCHEDULED|DEADLINE|CLOSED)(?=:)/gi)) addMatchRange(ranges, match, 1);
  for (const match of text.matchAll(/\b(https?|ftp|ftps|file|mailto|tel|data|irc|ircs|ssh|git|magnet)(?=:)/gi)) addMatchRange(ranges, match, 1);
  for (const match of text.matchAll(/#\+(BEGIN|END)(?=_)/gi)) addMatchRange(ranges, match, 1);
  for (const match of text.matchAll(/#\+(TITLE|ALIAS|TAGS|FILETAGS|ROAM_TAGS|PROPERTY|PROPERTIES|OPTIONS|STARTUP|AUTHOR|DATE|NAME|CAPTION|RESULTS|ATTR_HTML|ATTR_ORG)(?=:)/gi)) {
    addMatchRange(ranges, match, 1);
  }
  for (const match of text.matchAll(/:([A-Za-z][A-Za-z0-9_.-]*):(?=\s*$)/gim)) {
    const name = match[1].toLowerCase();
    if (name === "properties" || name === "end") addMatchRange(ranges, match, 1);
  }
  for (const match of text.matchAll(/(?:^|\r?\n)[\t ]*([A-Za-z][A-Za-z0-9_.-]*)(?=::)/g)) {
    if (STANDARD_PROPERTIES.has(match[1].toLowerCase())) addMatchRange(ranges, match, 1);
  }

  ranges.sort((a, b) => a.start - b.start || a.end - b.end);
  return ranges;
}

function isProtected(start, end, ranges) {
  return ranges.some((range) => range.start <= start && range.end >= end);
}

function alphabetFor(ch) {
  if (ch >= "A" && ch <= "Z") return ALPHABETS.upper;
  if (ch >= "a" && ch <= "z") return ALPHABETS.lower;
  if (ch >= "0" && ch <= "9") return ALPHABETS.digit;
  switch (encoder.encode(ch).length) {
    case 2: return ALPHABETS.two;
    case 3: return ALPHABETS.three;
    case 4: return ALPHABETS.four;
    default: safeError("Managed text contains an unsupported character encoding.");
  }
}

class PseudonymMap {
  constructor(salt, forbiddenTokens, forbiddenUuids) {
    this.salt = salt;
    this.forbiddenTokens = forbiddenTokens;
    this.forbiddenUuids = forbiddenUuids;
    this.tokens = new Map();
    this.tokenOwners = new Map();
    this.uuids = new Map();
    this.uuidOwners = new Map();
  }

  digest(label, input, counter) {
    return createHmac("sha256", this.salt).update(label).update("\0").update(input).update("\0").update(String(counter)).digest();
  }

  token(token) {
    const known = this.tokens.get(token);
    if (known !== undefined) return known;
    const alphabets = [...token].map(alphabetFor);
    for (let counter = 0; counter < 100_000; counter += 1) {
      const digest = this.digest("token", token, counter);
      const candidate = alphabets.map((alphabet, index) => alphabet[digest[index % digest.length] % alphabet.length]).join("");
      const owner = this.tokenOwners.get(candidate);
      if (candidate === token || this.forbiddenTokens.has(candidate) || GENERATED_PUBLIC_WORDS.has(candidate.toLowerCase()) || (owner !== undefined && owner !== token)) continue;
      this.tokens.set(token, candidate);
      this.tokenOwners.set(candidate, token);
      return candidate;
    }
    safeError("A same-shape pseudonym could not be represented without a collision.");
  }

  uuid(uuid) {
    const normalized = uuid.toLowerCase();
    const known = this.uuids.get(normalized);
    if (known !== undefined) return known;
    for (let counter = 0; counter < 100_000; counter += 1) {
      const bytes = Buffer.from(this.digest("uuid", normalized, counter).subarray(0, 16));
      bytes[6] = (bytes[6] & 0x0f) | 0x40;
      bytes[8] = (bytes[8] & 0x3f) | 0x80;
      const hex = bytes.toString("hex");
      const candidate = `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
      const owner = this.uuidOwners.get(candidate);
      if (candidate === normalized || this.forbiddenUuids.has(candidate) || (owner !== undefined && owner !== normalized)) continue;
      this.uuids.set(normalized, candidate);
      this.uuidOwners.set(candidate, normalized);
      return candidate;
    }
    safeError("A UUID pseudonym could not be represented without a collision.");
  }

  transform(text, preserveGrammar = false) {
    const ranges = preserveGrammar ? grammarProtectedRanges(text) : [];
    let out = "";
    let cursor = 0;
    for (const match of text.matchAll(UUID_RE)) {
      out += this.transformWords(text.slice(cursor, match.index), cursor, ranges);
      out += this.uuid(match[0]);
      cursor = match.index + match[0].length;
    }
    out += this.transformWords(text.slice(cursor), cursor, ranges);
    return out;
  }

  transformWords(segment, offset, ranges) {
    return segment.replace(WORD_RE, (token, index) => isProtected(offset + index, offset + index + token.length, ranges) ? token : this.token(token));
  }
}

function portableComponent(component) {
  if (!component || component === "." || component === ".." || /[<>:"/\\|?*\u0000-\u001f]/u.test(component) || /[. ]$/u.test(component)) {
    safeError("An output path component is not portable.");
  }
  const stem = component.split(".")[0].toLowerCase();
  if (/^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/u.test(stem) || encoder.encode(component).length > 240) {
    safeError("An output path component is not portable.");
  }
}

function portablePathKey(parts) {
  return parts.map((part) => part.normalize("NFC").toLowerCase()).join("/");
}

function pathTokenStats(parts, text, state) {
  for (const value of [...parts, text]) {
    for (const match of value.matchAll(UUID_RE)) {
      state.occurrences += 1;
      state.values.add(match[0].toLowerCase());
    }
  }
}

async function readGraph(source) {
  let sourceStat;
  try {
    sourceStat = await lstat(source);
  } catch {
    safeError("The source must be an existing directory.");
  }
  if (sourceStat.isSymbolicLink() || !sourceStat.isDirectory()) safeError("The source must be an existing non-symbolic-link directory.");

  let canonicalSource;
  try {
    canonicalSource = await realpath(source);
  } catch {
    safeError("The source directory could not be resolved safely.");
  }

  const files = [];
  async function walk(directory, parts) {
    let children;
    try {
      children = await readdir(directory, { withFileTypes: true });
    } catch {
      safeError("The source directory could not be read safely.");
    }
    children.sort((a, b) => a.name.localeCompare(b.name));
    for (const child of children) {
      const childPath = join(directory, child.name);
      let stat;
      try {
        stat = await lstat(childPath);
      } catch {
        safeError("A source entry could not be inspected safely.");
      }
      if (stat.isSymbolicLink()) safeError("The source contains a symbolic link.");
      if (stat.isDirectory()) {
        if (OMITTED_DIRECTORIES.has(child.name)) continue;
        await walk(childPath, [...parts, child.name]);
        continue;
      }
      if (!stat.isFile() || !isManagedFile(child.name)) continue;
      let bytes;
      let text;
      try {
        bytes = await readFile(childPath);
        text = decoder.decode(bytes);
      } catch {
        safeError("A managed file is not valid UTF-8 text.");
      }
      if (text.includes("\0")) safeError("A managed file contains unsupported text.");
      files.push({ parts: [...parts, child.name], text, bytes: bytes.length });
    }
  }

  await walk(canonicalSource, []);
  return { canonicalSource, files };
}

function collectSourceVocabulary(files) {
  const tokens = new Set();
  const uuids = new Set();
  for (const file of files) {
    for (const value of [...file.parts, file.text]) {
      for (const match of value.matchAll(WORD_RE)) tokens.add(match[0]);
      for (const match of value.matchAll(UUID_RE)) uuids.add(match[0].toLowerCase());
    }
  }
  return { tokens, uuids };
}

function planExport(files, salt) {
  const vocabulary = collectSourceVocabulary(files);
  const mapper = new PseudonymMap(salt, vocabulary.tokens, vocabulary.uuids);
  const seenNodes = new Map();
  const uuidStats = { occurrences: 0, values: new Set() };
  const formatCounts = { md: 0, markdown: 0, org: 0 };
  let totalBytes = 0;
  let maximumDirectoryDepth = 0;

  const planned = files.map((file) => {
    const outputParts = file.parts.map((part, index) => {
      if (index === 0 && STRUCTURAL_ROOT_DIRECTORIES.has(part)) return part;
      if (index < file.parts.length - 1) return mapper.transform(part);
      const extension = extname(part);
      return `${mapper.transform(part.slice(0, part.length - extension.length))}${extension}`;
    });
    for (const part of outputParts) portableComponent(part);
    for (let index = 1; index <= outputParts.length; index += 1) {
      const key = portablePathKey(outputParts.slice(0, index));
      const sourceKey = file.parts.slice(0, index).join("\0");
      const prior = seenNodes.get(key);
      if (prior !== undefined && prior !== sourceKey) safeError("Two source paths would collide in the portable output.");
      seenNodes.set(key, sourceKey);
    }
    const text = mapper.transform(file.text, true);
    if (encoder.encode(text).length !== file.bytes) safeError("A managed file could not retain its UTF-8 byte length.");
    const format = extname(file.parts.at(-1)).slice(1).toLowerCase();
    formatCounts[format] += 1;
    totalBytes += file.bytes;
    maximumDirectoryDepth = Math.max(maximumDirectoryDepth, file.parts.length - 1);
    pathTokenStats(file.parts, file.text, uuidStats);
    return { outputParts, text };
  });

  return {
    planned,
    fileCount: files.length,
    totalBytes,
    formatCounts,
    maximumDirectoryDepth,
    uuidOccurrences: uuidStats.occurrences,
    distinctUuids: uuidStats.values.size,
  };
}

function elapsedMilliseconds(start) {
  return Number(process.hrtime.bigint() - start) / 1_000_000;
}

export function formatAnonymizationReport(summary, destinationLabel = summary.destination) {
  const formats = ["md", "markdown", "org"].map((format) => `${format}: ${summary.formatCounts[format]}`).join(", ");
  return [
    "Tine graph anonymization export",
    `Destination: ${destinationLabel}`,
    `Files: ${summary.fileCount} (${formats})`,
    `Total bytes: ${summary.totalBytes}`,
    `Maximum directory depth: ${summary.maximumDirectoryDepth}`,
    `UUID occurrences / distinct UUIDs: ${summary.uuidOccurrences} / ${summary.distinctUuids}`,
    `Elapsed: ${summary.elapsedMilliseconds.toFixed(1)} ms`,
    "Review this result before sharing. Anonymization reduces risk; it is not a formal privacy proof.",
  ].join("\n");
}

/**
 * Create a new anonymized graph export.  `salt` exists only to make synthetic
 * fixture tests deterministic; the CLI always supplies fresh random bytes and
 * never writes a salt or reverse map.
 */
export async function anonymizeGraph({ source, destination, salt = randomBytes(32) }) {
  const started = process.hrtime.bigint();
  if (typeof source !== "string" || typeof destination !== "string" || !source || !destination) {
    safeError("Both --source and --destination are required.");
  }
  const sourcePath = resolve(source);
  const destinationPath = resolve(destination);
  if (sourcePath === destinationPath || isContained(destinationPath, sourcePath)) {
    safeError("The destination must not be the source or a descendant of it.");
  }
  try {
    await lstat(destinationPath);
    safeError("The destination must not already exist.");
  } catch (error) {
    if (error instanceof AnonymizeError) throw error;
    if (error?.code !== "ENOENT") safeError("The destination could not be inspected safely.");
  }

  const { canonicalSource, files } = await readGraph(sourcePath);
  if (isContained(destinationPath, canonicalSource)) safeError("The destination must not be the source or a descendant of it.");
  const plan = planExport(files, salt);
  let createdDestination = false;
  try {
    await mkdir(destinationPath);
    createdDestination = true;
    for (const file of plan.planned) {
      const outputPath = resolve(destinationPath, ...file.outputParts);
      if (!isContained(outputPath, destinationPath)) safeError("An output path escaped the destination.");
      await mkdir(dirname(outputPath), { recursive: true });
      await writeFile(outputPath, file.text, { encoding: "utf8", flag: "wx" });
    }
    const summary = {
      destination: destinationPath,
      ...plan,
      elapsedMilliseconds: elapsedMilliseconds(started),
    };
    await writeFile(join(destinationPath, REPORT_NAME), `${formatAnonymizationReport(summary, "this directory")}\n`, { encoding: "utf8", flag: "wx" });
    return summary;
  } catch (error) {
    if (createdDestination) await rm(destinationPath, { recursive: true, force: true });
    if (error instanceof AnonymizeError) throw error;
    safeError("The export could not be written safely.");
  }
}

function help() {
  return [
    "Usage: npm run anonymize-graph -- --source <graph-directory> --destination <new-output-directory>",
    "",
    "Creates a local, structural reproduction containing only anonymized Markdown/Org text.",
    "The destination must not exist. No files are uploaded, and the source is never modified.",
    "Review the result before sharing: anonymization reduces risk but is not a formal privacy proof.",
  ].join("\n");
}

function parseArguments(args) {
  if (args.includes("--help") || args.includes("-h")) return { help: true };
  const values = {};
  for (let index = 0; index < args.length; index += 1) {
    const flag = args[index];
    if (flag !== "--source" && flag !== "--destination") safeError("Use --source and --destination, or --help.");
    const value = args[index + 1];
    if (!value || value.startsWith("--")) safeError(`${flag} requires a value.`);
    if (values[flag]) safeError(`${flag} was provided more than once.`);
    values[flag] = value;
    index += 1;
  }
  return { source: values["--source"], destination: values["--destination"] };
}

async function main() {
  try {
    const parsed = parseArguments(process.argv.slice(2));
    if (parsed.help) {
      process.stdout.write(`${help()}\n`);
      return;
    }
    const summary = await anonymizeGraph(parsed);
    process.stdout.write(`${formatAnonymizationReport(summary)}\n`);
  } catch (error) {
    const message = error instanceof AnonymizeError ? error.message : "The export could not be completed without risk.";
    process.stderr.write(`Anonymization failed: ${message}\n`);
    process.exitCode = 1;
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await main();
