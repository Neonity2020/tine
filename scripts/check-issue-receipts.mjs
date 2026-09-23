#!/usr/bin/env node
// Every issue a release claims to fix must carry a maintainer receipt.
//
// v0.6.984 fixed fifteen reported issues and eleven of them had no
// `fixed-on-master` receipt comment at all: the reporter learned nothing until
// the release, and the release close-out had to reconstruct from commits what
// should already have been written down. AGENTS.md §8 requires the receipt --
// root cause, the literal path exercised, the platform and evidence layer -- so
// this is an unenforced rule, which is the same shape as the unlinted
// `cargo fmt` and the ungated Rust suite.
//
// Network-dependent by nature, so it is a release-manager command rather than
// part of the offline readiness preflight. Run it before closing out a release.
//
//   node scripts/check-issue-receipts.mjs [version]
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { releaseSection } from "./release-readiness-lib.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** Issue numbers a release section claims to have fixed. */
export function claimedIssues(section) {
  const fixed = /###\s+Fixed\s*\n([\s\S]*?)(?=\n###\s|\n##\s|$)/.exec(section ?? "");
  if (!fixed) return [];
  return [...new Set([...fixed[1].matchAll(/GH\s*#(\d+)/g)].map((match) => Number(match[1])))]
    .sort((a, b) => a - b);
}

/**
 * Whether a comment reads as a fix receipt.
 *
 * Deliberately generous about wording and strict about authorship: only a
 * repository collaborator's comment is a receipt, and any of the three things
 * §8 asks a receipt to say is enough to recognise it. The failing case this
 * must catch is an issue with NO maintainer comment between the report and the
 * release, which is what actually happened eleven times.
 */
export function hasReceipt(comments, maintainers) {
  return comments.some((comment) =>
    maintainers.has(comment.author)
    && /fixed on `?master`?|fixed-on-master|expected in the next release|should be fixed in v/i.test(comment.body ?? ""));
}

function gh(args) {
  const result = spawnSync("gh", args, { encoding: "utf8" });
  if (result.status !== 0) throw new Error(`gh ${args.join(" ")} failed: ${result.stderr?.trim()}`);
  return JSON.parse(result.stdout);
}

function main() {
  const version = process.argv[2]
    ?? JSON.parse(fs.readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8")).version;
  const section = releaseSection(fs.readFileSync(path.join(root, "CHANGELOG.md"), "utf8"), version);
  if (!section) {
    console.error(`CHANGELOG.md has no released section for ${version}`);
    process.exit(1);
  }
  const issues = claimedIssues(section);
  if (issues.length === 0) {
    console.log(`Issue receipts OK: v${version} claims no GH issue fixes.`);
    return;
  }
  const missing = [];
  for (const number of issues) {
    const issue = gh([
      "issue", "view", String(number), "--repo", "martinkoutecky/tine",
      "--json", "number,title,comments,author",
    ]);
    const maintainers = new Set(
      issue.comments
        .filter((comment) => ["OWNER", "MEMBER", "COLLABORATOR"].includes(comment.authorAssociation))
        .map((comment) => comment.author?.login)
        .filter(Boolean),
    );
    const comments = issue.comments.map((comment) => ({
      author: comment.author?.login,
      body: comment.body,
    }));
    if (!hasReceipt(comments, maintainers)) missing.push(`GH #${number} — ${issue.title}`);
  }
  if (missing.length > 0) {
    console.error(
      `v${version} claims ${issues.length} issue fix(es); ${missing.length} carry no maintainer receipt:`,
    );
    for (const entry of missing) console.error(`  ${entry}`);
    console.error(
      "\nAGENTS.md §8: a receipt names the root cause/invariant, the literal end-to-end path"
      + " exercised, the platform and evidence layer, and anything still unverified. Post them"
      + " before the release close-out, not from reconstructed commits afterwards.",
    );
    process.exit(1);
  }
  console.log(`Issue receipts OK: all ${issues.length} issue(s) claimed by v${version} carry one.`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
