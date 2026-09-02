#!/usr/bin/env node

// Count simple-query grammar shapes without emitting graph content or paths.
//
//   node scripts/harvest-query-census.mjs \
//     --graph tine-test=/path/to/tine-test \
//     --graph kitchen-sink=src/fixtures/kitchen-sink.md \
//     --graph org-graph=/path/to/org-graph \
//     --graph anonymized=/path/to/logseq-anonymized

import fs from "node:fs";
import path from "node:path";

const productions = [
  "PageRef",
  "Task",
  "Priority",
  "Property",
  "Scheduled",
  "Deadline",
  "Journal",
  "Between",
  "Page",
  "Namespace",
  "PageProperty",
  "PageTags",
  "Content",
  "Search",
  "ContentRegex",
  "And",
  "Or",
  "Not",
  "Sample",
  "SortBy",
  "Aggregate",
  "GroupBy",
];

const headProduction = new Map([
  ["and", "And"],
  ["or", "Or"],
  ["not", "Not"],
  ["task", "Task"],
  ["todo", "Task"],
  ["priority", "Priority"],
  ["page-ref", "PageRef"],
  ["tag", "PageRef"],
  ["property", "Property"],
  ["scheduled", "Scheduled"],
  ["deadline", "Deadline"],
  ["journal", "Journal"],
  ["between", "Between"],
  ["page", "Page"],
  ["namespace", "Namespace"],
  ["page-property", "PageProperty"],
  ["page-tags", "PageTags"],
  ["tags", "PageTags"],
  ["search", "Search"],
  ["content-regex", "ContentRegex"],
  ["sample", "Sample"],
  ["sort-by", "SortBy"],
  ["aggregate", "Aggregate"],
  ["group-by", "GroupBy"],
]);

function graphArguments() {
  const graphs = [];
  for (let index = 2; index < process.argv.length; index += 1) {
    if (process.argv[index] !== "--graph" || !process.argv[index + 1]) {
      throw new Error("usage: --graph label=/path (repeat for each corpus)");
    }
    const argument = process.argv[index + 1];
    const separator = argument.indexOf("=");
    if (separator <= 0 || separator === argument.length - 1) {
      throw new Error("each --graph must be label=/path");
    }
    graphs.push({ label: argument.slice(0, separator), root: path.resolve(argument.slice(separator + 1)) });
    index += 1;
  }
  if (graphs.length === 0) throw new Error("at least one --graph label=/path is required");
  if (new Set(graphs.map(({ label }) => label)).size !== graphs.length) {
    throw new Error("graph labels must be unique");
  }
  return graphs;
}

function graphFiles(root) {
  const stat = fs.lstatSync(root);
  if (stat.isSymbolicLink()) return [];
  if (stat.isFile()) return /\.(?:md|markdown|org)$/i.test(root) ? [root] : [];
  if (!stat.isDirectory()) return [];
  const files = [];
  const pending = [root];
  while (pending.length > 0) {
    const directory = pending.pop();
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      if (entry.isSymbolicLink()) continue;
      const candidate = path.join(directory, entry.name);
      if (entry.isDirectory()) {
        if (!entry.name.startsWith(".") && entry.name !== "node_modules") pending.push(candidate);
      } else if (entry.isFile() && /\.(?:md|markdown|org)$/i.test(entry.name)) {
        files.push(candidate);
      }
    }
  }
  return files;
}

function extractQuerySources(source) {
  const queries = [];
  const lower = source.toLowerCase();
  let cursor = 0;
  while (true) {
    const start = lower.indexOf("{{query", cursor);
    if (start < 0) break;
    const end = source.indexOf("}}", start + 7);
    if (end < 0) break;
    const query = source.slice(start + 7, end).trim();
    if (query) queries.push(query);
    cursor = end + 2;
  }

  const lines = source.split(/\r?\n/);
  for (let index = 0; index < lines.length; index += 1) {
    const begin = lines[index].match(/^\s*#\+BEGIN_QUERY\b(.*)$/i);
    if (!begin) continue;
    const body = [];
    if (begin[1].trim()) body.push(begin[1].trim());
    index += 1;
    while (index < lines.length && !/^\s*#\+END_QUERY\b/i.test(lines[index])) {
      body.push(lines[index]);
      index += 1;
    }
    queries.push(body.join("\n").trim());
  }
  return queries;
}

function tokenize(source) {
  const tokens = [];
  for (let index = 0; index < source.length; ) {
    const char = source[index];
    if (/\s/.test(char)) {
      index += 1;
    } else if (char === "(" || char === ")") {
      tokens.push({ kind: char });
      index += 1;
    } else if (char === "[" && source[index + 1] === "[") {
      const end = source.indexOf("]]", index + 2);
      tokens.push({ kind: "page-ref" });
      index = end < 0 ? source.length : end + 2;
    } else if (char === "#") {
      if (source[index + 1] === "[" && source[index + 2] === "[") {
        const end = source.indexOf("]]", index + 3);
        tokens.push({ kind: "tag" });
        index = end < 0 ? source.length : end + 2;
      } else {
        let end = index + 1;
        while (end < source.length && !/[\s()]/.test(source[end])) end += 1;
        tokens.push({ kind: "tag" });
        index = end;
      }
    } else if (char === '"') {
      let end = index + 1;
      while (end < source.length) {
        if (source[end] === "\\") end += 2;
        else if (source[end] === '"') {
          end += 1;
          break;
        } else end += 1;
      }
      tokens.push({ kind: "string" });
      index = end;
    } else {
      let end = index;
      while (end < source.length && !/[\s()]/.test(source[end])) end += 1;
      tokens.push({ kind: "word", value: source.slice(index, end).toLowerCase() });
      index = end;
    }
  }
  return tokens;
}

function classify(source) {
  const trimmed = source.trim();
  if (trimmed.startsWith("[:find") || trimmed.includes(":where") || trimmed.includes(":find")) {
    return { rows: new Set(), occurrences: new Map(), status: "advanced" };
  }
  const tokens = tokenize(trimmed);
  const rows = new Set();
  const occurrences = new Map();
  const note = (production) => {
    rows.add(production);
    occurrences.set(production, (occurrences.get(production) ?? 0) + 1);
  };
  let position = 0;
  function skipExpression() {
    if (tokens[position]?.kind !== "(") {
      position += 1;
      return;
    }
    let depth = 0;
    do {
      if (tokens[position]?.kind === "(") depth += 1;
      if (tokens[position]?.kind === ")") depth -= 1;
      position += 1;
    } while (position < tokens.length && depth > 0);
  }
  function expression() {
    const token = tokens[position];
    if (!token) return false;
    if (token.kind === "page-ref" || token.kind === "tag") {
      note("PageRef");
      position += 1;
      return true;
    }
    if (token.kind === "string" || token.kind === "word") {
      note("Content");
      position += 1;
      return true;
    }
    if (token.kind !== "(") return false;
    position += 1;
    const head = tokens[position];
    if (head?.kind !== "word") return false;
    position += 1;
    const production = headProduction.get(head.value);
    if (!production) return false;
    note(production);
    if (production === "And" || production === "Or") {
      while (position < tokens.length && tokens[position].kind !== ")") {
        if (!expression()) skipExpression();
      }
    } else if (production === "Not") {
      if (!expression()) skipExpression();
      while (position < tokens.length && tokens[position].kind !== ")") skipExpression();
    } else {
      while (position < tokens.length && tokens[position].kind !== ")") skipExpression();
    }
    if (tokens[position]?.kind === ")") position += 1;
    return true;
  }
  const valid = expression() && position === tokens.length;
  return { rows, occurrences, status: valid ? "simple" : "invalid" };
}

const output = { schemaVersion: 1, productions, graphs: [] };
for (const { label, root } of graphArguments()) {
  if (!fs.existsSync(root)) throw new Error(`missing graph input for label ${label}`);
  const files = graphFiles(root);
  const rowQueries = Object.fromEntries(productions.map((production) => [production, 0]));
  const rowOccurrences = Object.fromEntries(productions.map((production) => [production, 0]));
  let queries = 0;
  let simple = 0;
  let advanced = 0;
  let invalid = 0;
  for (const file of files) {
    const source = fs.readFileSync(file, "utf8");
    for (const query of extractQuerySources(source)) {
      queries += 1;
      const classified = classify(query);
      if (classified.status === "simple") simple += 1;
      else if (classified.status === "advanced") advanced += 1;
      else invalid += 1;
      for (const production of classified.rows) rowQueries[production] += 1;
      for (const [production, count] of classified.occurrences) rowOccurrences[production] += count;
    }
  }
  output.graphs.push({ label, files: files.length, queries, simple, advanced, invalid, rowQueries, rowOccurrences });
}

process.stdout.write(`${JSON.stringify(output, null, 2)}\n`);
