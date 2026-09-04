#!/usr/bin/env node
// Validates the W4-C7b lane-evidence artifacts named by the packet's §2a.
//
// The evidence files themselves are gitignored lane evidence at the worktree
// root: this checker asserts their SCHEMA, so a receipt that silently lost a
// required heading, a measurement table that lost a column, or a necessity log
// that recorded no failing command cannot pass review unnoticed. It is not a
// release gate; run it from the repository root before handing the lane over.
//
// Exit 0 = every required artifact is present and well-formed.

import { readFileSync, existsSync } from 'node:fs';
import { resolve } from 'node:path';

const root = process.cwd();
const problems = [];

function read(name) {
  const path = resolve(root, name);
  if (!existsSync(path)) {
    problems.push(`missing artifact: ${name}`);
    return null;
  }
  const text = readFileSync(path, 'utf8');
  if (text.trim().length === 0) {
    problems.push(`empty artifact: ${name}`);
    return null;
  }
  return text;
}

// 1. RECEIPT.md carries every §2a heading, in order, each with a body.
const RECEIPT_HEADINGS = [
  'Base',
  'Necessity',
  'Measurements',
  'Cuts and measured exceptions',
  'Artifacts',
  'Gates',
  'Forks',
  'Write-set escapes',
];
const receipt = read('RECEIPT.md');
if (receipt) {
  const headings = [...receipt.matchAll(/^#{1,6}\s+(.+?)\s*$/gm)].map((match) => match[1]);
  let cursor = -1;
  for (const required of RECEIPT_HEADINGS) {
    const index = headings.findIndex((heading, at) => at > cursor && heading === required);
    if (index === -1) {
      problems.push(`RECEIPT.md: missing or out-of-order heading "${required}"`);
    } else {
      cursor = index;
    }
  }
  for (const required of RECEIPT_HEADINGS) {
    const pattern = new RegExp(
      `^#{1,6}\\s+${required.replace(/[.*+?^${}()|[\\]\\\\]/g, '\\\\$&')}\\s*$([\\s\\S]*?)(?=^#{1,6}\\s|\\Z)`,
      'm',
    );
    const section = receipt.match(pattern);
    if (section && section[1].trim().length === 0) {
      problems.push(`RECEIPT.md: heading "${required}" has an empty body`);
    }
  }
}

// 2. The measurement table carries every required column.
const MEASUREMENT_COLUMNS = [
  'cluster',
  'operation',
  'backend',
  'phase',
  'sample / rounds',
  'before counters',
  'after counters',
  'median',
  'p95',
  'decision',
  'evidence file',
];
const measurements = read('measurements-w4-c7b.md');
if (measurements) {
  const header = measurements
    .split('\n')
    .find((line) => line.startsWith('|') && line.toLowerCase().includes('cluster'));
  if (!header) {
    problems.push('measurements-w4-c7b.md: no measurement table header row');
  } else {
    const columns = header
      .split('|')
      .map((column) => column.trim().toLowerCase())
      .filter((column) => column.length > 0);
    for (const required of MEASUREMENT_COLUMNS) {
      if (!columns.includes(required)) {
        problems.push(`measurements-w4-c7b.md: table is missing column "${required}"`);
      }
    }
    const rows = measurements
      .split('\n')
      .filter((line) => line.startsWith('|') && !line.includes('---') && line !== header);
    if (rows.length === 0) {
      problems.push('measurements-w4-c7b.md: the table has no data rows');
    }
  }
  for (const cited of measurements.matchAll(/measurement[\w.-]*\.txt/g)) {
    if (!existsSync(resolve(root, cited[0]))) {
      problems.push(`measurements-w4-c7b.md cites a missing raw file: ${cited[0]}`);
    }
  }
}

// 3. The baseline names its failing tests, and the necessity evidence records at
//    least one command whose output actually failed. A necessity log that shows
//    only passes proves nothing.
const baseline = read('baseline-tine-core-lib.txt');
if (baseline && !/test result:/.test(baseline)) {
  problems.push('baseline-tine-core-lib.txt: no "test result:" line');
}

const necessity = ['c7b-source-scans-fail-before.log'];
for (const name of necessity) {
  const text = read(name);
  if (text && !/FAILED|panicked|assertion/.test(text)) {
    problems.push(`${name}: records no failing run, so it is not fail-before evidence`);
  }
}

if (problems.length > 0) {
  console.error('W4-C7b evidence check FAILED:');
  for (const problem of problems) {
    console.error(`  - ${problem}`);
  }
  process.exit(1);
}
console.log('W4-C7b evidence OK: receipt headings, measurement columns, baseline and necessity logs.');
