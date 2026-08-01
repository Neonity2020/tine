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
const STRUCTURAL_ROOT_DIRECTORIES = new Set(["pages", "journals"]);
const REPORT_NAME = "anonymization-report.txt";
const WORD_RE = /[\p{L}\p{M}\p{N}]+/gu;
const UUID_RE = /(?<![\p{L}\p{N}_])[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}(?![\p{L}\p{N}_])/giu;
const MAX_HIDDEN_EDN_BYTES = 64 * 1024;
const MAX_HIDDEN_EDN_ENTRIES = 1024;
const MAX_HIDDEN_EDN_DEPTH = 64;
const MAX_HIDDEN_EDN_FORMS = 4096;

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

const ASCII_ALPHABETS = {
  upper: { key: "upper", values: [..."ABCDEFGHIJKLMNOPQRSTUVWXYZ"], valueSet: new Set("ABCDEFGHIJKLMNOPQRSTUVWXYZ") },
  lower: { key: "lower", values: [..."abcdefghijklmnopqrstuvwxyz"], valueSet: new Set("abcdefghijklmnopqrstuvwxyz") },
  digit: { key: "digit", values: [..."0123456789"], valueSet: new Set("0123456789") },
};
const UNICODE_WORD_CHARACTER_RE = /^[\p{L}\p{M}\p{N}]$/u;
let unicodeAlphabets;

export class AnonymizeError extends Error {
  constructor(message) {
    super(message);
    this.name = "AnonymizeError";
  }
}

function isManagedFile(name) {
  return MANAGED_EXTENSIONS.has(extname(name).toLowerCase());
}

function componentsStartWith(parts, prefix) {
  return parts.length >= prefix.length && prefix.every((part, index) => parts[index].toLowerCase() === part);
}

function isFixedExcluded(parts) {
  const lowered = parts.map((part) => part.toLowerCase());
  return lowered.includes("node_modules")
    || componentsStartWith(parts, ["assets"])
    || componentsStartWith(parts, ["publish"])
    || componentsStartWith(parts, [".tine-sync"])
    || componentsStartWith(parts, ["logseq", ".recycle"])
    || componentsStartWith(parts, ["logseq", "bak"])
    || componentsStartWith(parts, ["logseq", "version-files"])
    || componentsStartWith(parts, ["logseq", ".tine-trash"]);
}

function isProviderConflictCopy(filename) {
  const extension = extname(filename);
  const stem = extension ? filename.slice(0, -extension.length) : filename;
  return stem.includes(".sync-conflict-") || (stem.includes(" (") && stem.includes("conflicted copy"));
}

function componentIsManagedPortable(component) {
  if (!component || component === "." || component === ".." || /[<>:"\\|?*\u0000-\u001f\u007f-\u009f]/u.test(component) || /[. ]$/u.test(component)) {
    return false;
  }
  const stem = component.split(".", 1)[0].toUpperCase();
  return !/^(?:CON|PRN|AUX|NUL|COM[1-9¹²³]|LPT[1-9¹²³])$/u.test(stem);
}

function matchesHiddenPrefix(parts, hiddenPrefixes) {
  const relative = parts.join("/");
  return hiddenPrefixes.some((prefix) => relative.startsWith(prefix));
}

function shouldDescend(parts, hiddenPrefixes = []) {
  return parts.every(componentIsManagedPortable)
    && !parts.some((part) => part.startsWith("."))
    && !isFixedExcluded(parts)
    && !matchesHiddenPrefix(parts, hiddenPrefixes);
}

function isGraphTextEligible(parts, hiddenPrefixes = []) {
  const filename = parts.at(-1);
  return shouldDescend(parts.slice(0, -1), hiddenPrefixes)
    && componentIsManagedPortable(filename)
    && !filename.startsWith(".")
    && !isFixedExcluded(parts)
    && !matchesHiddenPrefix(parts, hiddenPrefixes)
    && !isProviderConflictCopy(filename)
    && isManagedFile(filename);
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
  const merged = [];
  for (const range of ranges) {
    const prior = merged.at(-1);
    if (prior && range.start <= prior.end) prior.end = Math.max(prior.end, range.end);
    else merged.push({ ...range });
  }
  return merged;
}

function isProtected(start, end, ranges, cursor, work) {
  while (cursor.index < ranges.length) {
    work.comparisons += 1;
    if (ranges[cursor.index].end > start) break;
    cursor.index += 1;
  }
  if (cursor.index >= ranges.length) return false;
  work.comparisons += 1;
  return ranges[cursor.index].start <= start && ranges[cursor.index].end >= end;
}

function alphabetFor(ch) {
  if (ch >= "A" && ch <= "Z") return ASCII_ALPHABETS.upper;
  if (ch >= "a" && ch <= "z") return ASCII_ALPHABETS.lower;
  if (ch >= "0" && ch <= "9") return ASCII_ALPHABETS.digit;
  const byteLength = encoder.encode(ch).length;
  if (byteLength < 2 || byteLength > 4) safeError("Managed text contains an unsupported character encoding.");
  if (unicodeAlphabets === undefined) unicodeAlphabets = buildUnicodeAlphabets();
  return unicodeAlphabets.get(byteLength);
}

function buildUnicodeAlphabets() {
  const valuesByLength = new Map([[2, []], [3, []], [4, []]]);
  // A single representative for each conservative portable identity prevents
  // generated path components from differing only by case or normalization.
  const portableIdentities = new Set(
    [..."ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"].map(conservativePortableFold),
  );
  for (let codePoint = 0x80; codePoint <= 0x10ffff; codePoint += 1) {
    if (codePoint >= 0xd800 && codePoint <= 0xdfff) continue;
    const character = String.fromCodePoint(codePoint);
    if (!UNICODE_WORD_CHARACTER_RE.test(character) || character.normalize("NFC") !== character) continue;
    const identity = conservativePortableFold(character);
    if (portableIdentities.has(identity) || !componentIsManagedPortable(character)) continue;
    portableIdentities.add(identity);
    valuesByLength.get(encoder.encode(character).length)?.push(character);
  }
  return new Map([...valuesByLength].map(([byteLength, values]) => [
    byteLength,
    { key: `utf8-${byteLength}`, values, valueSet: new Set(values) },
  ]));
}

function greatestCommonDivisor(left, right) {
  while (right !== 0n) [left, right] = [right, left % right];
  return left;
}

class PseudonymMap {
  constructor(salt, forbiddenTokens, forbiddenUuids) {
    this.salt = salt;
    this.forbiddenTokens = forbiddenTokens;
    this.forbiddenUuids = forbiddenUuids;
    this.tokens = new Map();
    this.uuids = new Map();
    this.uuidOwners = new Map();
    this.protectedRangeWork = { comparisons: 0 };
    this.assignTokens();
  }

  digest(label, input, counter) {
    return createHmac("sha256", this.salt).update(label).update("\0").update(input).update("\0").update(String(counter)).digest();
  }

  saltedOrder(label, owner, values) {
    return [...values]
      .map((value) => ({ value, digest: this.digest(label, `${owner}\0${value}`, 0) }))
      .sort((left, right) => Buffer.compare(left.digest, right.digest) || left.value.localeCompare(right.value))
      .map(({ value }) => value);
  }

  digestInteger(label, input) {
    return BigInt(`0x${this.digest(label, input, 0).toString("hex")}`);
  }

  *shapeCandidates(shapeKey, alphabets) {
    const sizes = alphabets.map(({ values }) => BigInt(values.length));
    const domainSize = sizes.reduce((product, size) => product * size, 1n);
    if (domainSize === 0n) return;
    let index = this.digestInteger("shape-start", shapeKey) % domainSize;
    let step = domainSize === 1n ? 1n : this.digestInteger("shape-step", shapeKey) % domainSize;
    if (step === 0n) step = 1n;
    while (greatestCommonDivisor(step, domainSize) !== 1n) {
      step = step + 1n === domainSize ? 1n : step + 1n;
    }
    for (let visited = 0n; visited < domainSize; visited += 1n) {
      let remaining = index;
      const characters = Array(alphabets.length);
      for (let position = alphabets.length - 1; position >= 0; position -= 1) {
        characters[position] = alphabets[position].values[Number(remaining % sizes[position])];
        remaining /= sizes[position];
      }
      yield characters.join("");
      index = (index + step) % domainSize;
    }
  }

  candidateIsAdmissible(candidate) {
    return candidate.normalize("NFC") === candidate
      && componentIsManagedPortable(candidate)
      && !GENERATED_PUBLIC_WORDS.has(candidate.toLowerCase());
  }

  assignGroup(shapeKey, alphabets, groupTokens) {
    const owners = this.saltedOrder("shape-owner", shapeKey, groupTokens);
    const absentCandidates = [];
    for (const candidate of this.shapeCandidates(shapeKey, alphabets)) {
      if (!this.candidateIsAdmissible(candidate) || this.forbiddenTokens.has(candidate)) continue;
      absentCandidates.push(candidate);
      if (absentCandidates.length === owners.length) break;
    }

    const sourceCandidates = this.saltedOrder(
      "shape-source-candidate",
      shapeKey,
      groupTokens.filter((token) => [...token].every(
        (character, index) => alphabets[index].valueSet.has(character),
      ) && this.candidateIsAdmissible(token)),
    );
    const neededSourceCandidates = owners.length - absentCandidates.length;
    if (sourceCandidates.length < neededSourceCandidates) {
      safeError("A same-shape pseudonym could not be represented without a collision.");
    }
    const selected = this.saltedOrder(
      "shape-selected-candidate",
      shapeKey,
      [...absentCandidates, ...sourceCandidates.slice(0, neededSourceCandidates)],
    );

    const fixed = [];
    for (let index = 0; index < owners.length; index += 1) {
      if (owners[index] === selected[index]) fixed.push(index);
    }
    if (fixed.length > 1) {
      const candidates = fixed.map((index) => selected[index]);
      for (let index = 0; index < fixed.length; index += 1) {
        selected[fixed[index]] = candidates[(index + 1) % candidates.length];
      }
    } else if (fixed.length === 1) {
      const other = owners.length === 1 ? -1 : (fixed[0] === 0 ? 1 : 0);
      if (other < 0) safeError("A same-shape pseudonym could not be represented without a collision.");
      [selected[fixed[0]], selected[other]] = [selected[other], selected[fixed[0]]];
    }
    for (let index = 0; index < owners.length; index += 1) {
      if (owners[index] === selected[index]) safeError("A same-shape pseudonym could not be represented without a collision.");
      this.tokens.set(owners[index], selected[index]);
    }
  }

  assignTokens() {
    const groups = new Map();
    for (const token of this.forbiddenTokens) {
      const alphabets = [...token].map(alphabetFor);
      const shapeKey = alphabets.map(({ key }) => key).join("/");
      const group = groups.get(shapeKey) ?? { alphabets, tokens: [] };
      group.tokens.push(token);
      groups.set(shapeKey, group);
    }
    for (const [shapeKey, { alphabets, tokens }] of groups) this.assignGroup(shapeKey, alphabets, tokens);
  }

  token(token) {
    const known = this.tokens.get(token);
    if (known !== undefined) return known;
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
    const rangeCursor = { index: 0 };
    let out = "";
    let cursor = 0;
    for (const match of text.matchAll(UUID_RE)) {
      out += this.transformWords(text.slice(cursor, match.index), cursor, ranges, rangeCursor);
      out += this.uuid(match[0]);
      cursor = match.index + match[0].length;
    }
    out += this.transformWords(text.slice(cursor), cursor, ranges, rangeCursor);
    return out;
  }

  transformWords(segment, offset, ranges, rangeCursor) {
    return segment.replace(WORD_RE, (token, index) => isProtected(
      offset + index,
      offset + index + token.length,
      ranges,
      rangeCursor,
      this.protectedRangeWork,
    ) ? token : this.token(token));
  }
}

function portableComponent(component) {
  if (!componentIsManagedPortable(component)) {
    safeError("An output path component is not portable.");
  }
  if (encoder.encode(component).length > 240) {
    safeError("An output path component is not portable.");
  }
}

function conservativePortableFold(component) {
  // Upper-then-lower expands the full-fold cases that plain lowercasing misses
  // (ß/ss, final sigma, ligatures). It can conservatively collapse additional
  // rare spellings, which makes this exporter refuse rather than split identity.
  return component.normalize("NFD").toUpperCase().toLowerCase().normalize("NFC");
}

function portablePathKey(parts) {
  return parts.map(conservativePortableFold).join("/");
}

function pathTokenStats(parts, text, state) {
  for (const value of [...parts, text]) {
    for (const match of value.matchAll(UUID_RE)) {
      state.occurrences += 1;
      state.values.add(match[0].toLowerCase());
    }
  }
}

class EdnReader {
  constructor(source) {
    this.source = source;
    this.pos = 0;
    this.depth = 0;
    this.forms = 0;
  }

  peek(offset = 0) {
    return this.source[this.pos + offset];
  }

  fail() {
    throw new Error("invalid EDN");
  }

  skipInterstitial() {
    while (true) {
      while ([" ", "\t", "\n", "\r", ","].includes(this.peek())) this.pos += 1;
      if (this.peek() !== ";") return;
      while (this.peek() !== undefined && this.peek() !== "\n") this.pos += 1;
    }
  }

  skipInterstitialAndDiscards() {
    while (true) {
      this.skipInterstitial();
      if (this.peek() !== "#" || this.peek(1) !== "_") return;
      this.withForm(() => {
        this.pos += 2;
        this.skipForm();
      });
    }
  }

  beginForm() {
    this.forms += 1;
    this.depth += 1;
    if (this.forms > MAX_HIDDEN_EDN_FORMS || this.depth > MAX_HIDDEN_EDN_DEPTH) this.fail();
  }

  endForm() {
    this.depth -= 1;
  }

  withForm(action) {
    this.beginForm();
    try {
      return action();
    } finally {
      this.endForm();
    }
  }

  skipForm() {
    this.skipInterstitialAndDiscards();
    return this.withForm(() => this.skipFormBody());
  }

  skipFormBody() {
    const next = this.peek();
    if (next === undefined) this.fail();
    if (next === "\"") return void this.scanString(false);
    if (next === "[") return this.skipCollection("]", false);
    if (next === "{") return this.skipCollection("}", true);
    if (next === "(") return this.skipCollection(")", false);
    if (next === "#" && this.peek(1) === "{") {
      this.pos += 1;
      return this.skipCollection("}", false);
    }
    if (next === "#" && this.peek(1) === "(") {
      this.pos += 1;
      return this.skipCollection(")", false);
    }
    if (next === "#" && this.peek(1) === "\"") {
      this.pos += 1;
      return void this.scanString(false);
    }
    if (next === "#" && this.peek(1) === "'") {
      this.pos += 2;
      return this.skipForm();
    }
    if (next === "#" && this.peek(1) === "#") return this.skipAtom();
    if (next === "#") {
      this.pos += 1;
      this.skipAtom();
      return this.skipForm();
    }
    if (["'", "`", "@"].includes(next)) {
      this.pos += 1;
      return this.skipForm();
    }
    if (next === "~") {
      this.pos += 1;
      if (this.peek() === "@") this.pos += 1;
      return this.skipForm();
    }
    if (next === "^") {
      this.pos += 1;
      this.skipForm();
      return this.skipForm();
    }
    if (next === "\\") return this.skipCharacter();
    if (["]", "}", ")"].includes(next)) this.fail();
    return this.skipAtom();
  }

  skipCollection(close, map) {
    this.pos += 1;
    let forms = 0;
    while (true) {
      this.skipInterstitialAndDiscards();
      if (this.peek() === close) {
        this.pos += 1;
        if (map && forms % 2 !== 0) this.fail();
        return;
      }
      if (this.peek() === undefined) this.fail();
      this.skipForm();
      forms += 1;
    }
  }

  skipAtom() {
    const start = this.pos;
    const delimiters = new Set([" ", "\t", "\n", "\r", ",", ";", "\"", "[", "]", "{", "}", "(", ")"]);
    while (this.peek() !== undefined && !delimiters.has(this.peek())) this.pos += 1;
    if (this.pos === start) this.fail();
  }

  skipCharacter() {
    this.pos += 1;
    const codePoint = this.source.codePointAt(this.pos);
    if (codePoint === undefined) this.fail();
    this.pos += codePoint > 0xffff ? 2 : 1;
    const delimiters = new Set([" ", "\t", "\n", "\r", ",", ";", "[", "]", "{", "}", "(", ")"]);
    while (this.peek() !== undefined && !delimiters.has(this.peek())) this.pos += 1;
  }

  scanString(decode) {
    if (this.peek() !== "\"") this.fail();
    this.pos += 1;
    let output = "";
    while (true) {
      const next = this.peek();
      if (next === undefined) this.fail();
      if (next === "\"") {
        this.pos += 1;
        return decode ? output : undefined;
      }
      if (next === "\\") {
        this.pos += 1;
        const escaped = this.peek();
        this.pos += 1;
        const simple = { "\"": "\"", "\\": "\\", n: "\n", r: "\r", t: "\t", b: "\b", f: "\f" };
        let character = simple[escaped];
        if (character === undefined && escaped === "u") {
          const digits = this.source.slice(this.pos, this.pos + 4);
          if (!/^[0-9a-f]{4}$/iu.test(digits)) this.fail();
          const value = Number.parseInt(digits, 16);
          if (value >= 0xd800 && value <= 0xdfff) this.fail();
          character = String.fromCodePoint(value);
          this.pos += 4;
        } else if (character === undefined) {
          this.fail();
        }
        if (decode) output += character;
        continue;
      }
      const codePoint = this.source.codePointAt(this.pos);
      const character = String.fromCodePoint(codePoint);
      this.pos += character.length;
      if (decode) output += character;
    }
  }

  readHiddenValue() {
    const start = this.pos;
    this.skipInterstitialAndDiscards();
    if (this.peek() !== "[") {
      this.skipForm();
      if (encoder.encode(this.source.slice(start, this.pos)).length > MAX_HIDDEN_EDN_BYTES) this.fail();
      return [];
    }
    const values = this.withForm(() => {
      this.pos += 1;
      const values = [];
      let entries = 0;
      while (true) {
        this.skipInterstitialAndDiscards();
        if (this.peek() === "]") {
          this.pos += 1;
          return values;
        }
        if (this.peek() === undefined) this.fail();
        entries += 1;
        if (entries > MAX_HIDDEN_EDN_ENTRIES) this.fail();
        if (this.peek() === "\"") {
          values.push(this.withForm(() => this.scanString(true)));
        } else {
          this.skipForm();
        }
      }
    });
    if (encoder.encode(this.source.slice(start, this.pos)).length > MAX_HIDDEN_EDN_BYTES) this.fail();
    return values;
  }
}

function parseHiddenPaths(config) {
  let hiddenPaths;
  try {
    if (config.includes("\0")) throw new Error("invalid EDN");
    const reader = new EdnReader(config);
    reader.skipInterstitialAndDiscards();
    if (reader.peek() !== "{") throw new Error("invalid EDN");
    reader.beginForm();
    reader.pos += 1;
    try {
      while (true) {
        reader.skipInterstitialAndDiscards();
        if (reader.peek() === "}") {
          reader.pos += 1;
          break;
        }
        if (reader.peek() === undefined) throw new Error("invalid EDN");
        const start = reader.pos;
        const keyword = reader.peek() === ":";
        reader.skipForm();
        const key = keyword ? config.slice(start, reader.pos) : undefined;
        reader.skipInterstitialAndDiscards();
        if (reader.peek() === undefined || reader.peek() === "}") throw new Error("invalid EDN");
        if (key === ":hidden") {
          if (hiddenPaths !== undefined) throw new Error("duplicate hidden key");
          hiddenPaths = reader.readHiddenValue();
        } else {
          reader.skipForm();
        }
      }
      reader.skipInterstitialAndDiscards();
      if (reader.pos !== config.length) throw new Error("invalid EDN");
    } finally {
      reader.endForm();
    }
  } catch {
    safeError("The graph configuration or hidden policy could not be interpreted safely.");
  }

  const prefixes = [];
  let retainedBytes = 0;
  for (const entry of hiddenPaths ?? []) {
    if (entry === "") safeError("The graph configuration contains a hide-all hidden policy.");
    if (entry.startsWith("/")) continue;
    const normalized = entry.endsWith("/") ? entry.slice(0, -1) : entry;
    const components = normalized.split("/");
    if (normalized !== normalized.trim()
      || normalized.includes("\\")
      || normalized.includes("\0")
      || components.some((component) => !componentIsManagedPortable(component))) continue;
    retainedBytes += encoder.encode(normalized).length;
    if (retainedBytes > MAX_HIDDEN_EDN_BYTES) safeError("The graph configuration contains an oversized hidden policy.");
    prefixes.push(normalized);
  }
  return [...new Set(prefixes)];
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

  let hiddenPrefixes = [];
  const configPath = join(canonicalSource, "logseq", "config.edn");
  try {
    const configStat = await lstat(configPath);
    if (configStat.isSymbolicLink() || !configStat.isFile() || configStat.nlink !== 1) {
      safeError("The graph configuration could not be read safely.");
    }
    const config = decoder.decode(await readFile(configPath));
    hiddenPrefixes = parseHiddenPaths(config);
  } catch (error) {
    if (error instanceof AnonymizeError) throw error;
    if (error?.code !== "ENOENT") safeError("The graph configuration could not be read safely.");
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
      const childParts = [...parts, child.name];
      if (child.isDirectory() && !shouldDescend(childParts, hiddenPrefixes)) continue;
      if (!child.isDirectory() && !isGraphTextEligible(childParts, hiddenPrefixes)) continue;
      let stat;
      try {
        stat = await lstat(childPath);
      } catch {
        safeError("A source entry could not be inspected safely.");
      }
      if (stat.isSymbolicLink()) safeError("The source contains a symbolic link.");
      if (stat.isDirectory()) {
        if (!shouldDescend(childParts, hiddenPrefixes)) continue;
        await walk(childPath, childParts);
        continue;
      }
      if (!stat.isFile() || !isGraphTextEligible(childParts, hiddenPrefixes)) continue;
      if (stat.nlink !== 1) safeError("A managed source file has multiple hard links.");
      let bytes;
      let text;
      try {
        bytes = await readFile(childPath);
        text = decoder.decode(bytes);
      } catch {
        safeError("A managed file is not valid UTF-8 text.");
      }
      if (text.includes("\0")) safeError("A managed file contains unsupported text.");
      files.push({ parts: childParts, text, bytes: bytes.length });
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

function assertNoPortableSourceCollisions(files) {
  const seenNodes = new Map();
  for (const file of files) {
    for (let index = 1; index <= file.parts.length; index += 1) {
      const key = portablePathKey(file.parts.slice(0, index));
      const exact = file.parts.slice(0, index).join("\0");
      const prior = seenNodes.get(key);
      if (prior !== undefined && prior !== exact) {
        safeError("Two source paths have the same portable identity.");
      }
      seenNodes.set(key, exact);
    }
  }
}

function planExport(files, salt) {
  assertNoPortableSourceCollisions(files);
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
    protectedRangeComparisons: mapper.protectedRangeWork.comparisons,
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
    "Ordinary Logseq :hidden path prefixes are excluded before files or vocabulary are inventoried.",
    "Pseudonyms preserve character shapes and UTF-8 byte lengths, including saturated bounded domains.",
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
