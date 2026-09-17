#!/usr/bin/env node

// Fail closed unless GitHub Actions records a successful manually dispatched
// full CI run for the exact release-candidate commit. Focused and PR runs use
// the same workflow but cannot satisfy this gate because their full jobs are
// absent or skipped.

import { spawnSync } from "node:child_process";
import { collectGithubPages, selectExactCiEvidence } from "./ci-evidence-lib.mjs";

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

// Every input this gate needs is already sitting in the checkout, yet all three
// used to be typed by hand on every invocation — and a mistyped --sha does not
// fail loudly, it reports "no exact-SHA evidence", which reads exactly like a
// red CI run. Derive them, and say which value came from where, so a wrong
// answer is visible in the output instead of inferred.
function capture(command, args) {
  const result = spawnSync(command, args, { encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : undefined;
}

function derivedRepository() {
  const urls = capture("git", ["config", "--get-all", "remote.origin.url"])?.split(/\r?\n/) ?? [];
  for (const url of urls) {
    const match = /github\.com[:/]([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+?)(?:\.git)?$/.exec(url.trim());
    if (match) return `${match[1]}/${match[2]}`;
  }
  return undefined;
}

const shaSource = option("--sha") ? "--sha"
  : process.env.CI_EVIDENCE_SHA ? "CI_EVIDENCE_SHA"
  : process.env.GITHUB_SHA ? "GITHUB_SHA" : "git rev-parse HEAD";
const sha = (option("--sha") ?? process.env.CI_EVIDENCE_SHA ?? process.env.GITHUB_SHA
  ?? capture("git", ["rev-parse", "HEAD"]))?.trim().toLowerCase();
const repositorySource = option("--repo") ? "--repo"
  : process.env.CI_EVIDENCE_REPOSITORY ? "CI_EVIDENCE_REPOSITORY"
  : process.env.GITHUB_REPOSITORY ? "GITHUB_REPOSITORY" : "remote.origin.url";
const repository = option("--repo") ?? process.env.CI_EVIDENCE_REPOSITORY ?? process.env.GITHUB_REPOSITORY
  ?? derivedRepository();
const workflow = option("--workflow") ?? "ci.yml";
const token = process.env.GH_TOKEN ?? process.env.GITHUB_TOKEN ?? capture("gh", ["auth", "token"]);
const apiBase = process.env.GITHUB_API_URL ?? "https://api.github.com";

if (!sha) throw new Error("No candidate SHA: pass --sha, set CI_EVIDENCE_SHA/GITHUB_SHA, or run this from a git checkout.");
if (!/^[0-9a-f]{40}$/.test(sha)) throw new Error(`Candidate SHA must be a full 40-character commit: ${sha}`);
if (!repository || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) {
  throw new Error("No GitHub repository: pass --repo owner/name, set GITHUB_REPOSITORY, or add a github.com remote.origin.url.");
}
if (!token) {
  throw new Error("No GitHub token: run `gh auth login`, or set GH_TOKEN/GITHUB_TOKEN with Actions read permission.");
}
console.log(`Checking ${repository} (${repositorySource}) at ${sha} (${shaSource}) for a dispatched ${workflow} run.`);

async function github(path) {
  const response = await fetch(`${apiBase}${path}`, {
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: `Bearer ${token}`,
      "X-GitHub-Api-Version": "2022-11-28",
    },
  });
  if (!response.ok) throw new Error(`GitHub API ${response.status} for ${path}: ${await response.text()}`);
  return response.json();
}

const params = new URLSearchParams({
  event: "workflow_dispatch",
  head_sha: sha,
  status: "completed",
  per_page: "100",
});
const encodedWorkflow = encodeURIComponent(workflow);
const runsPath = `/repos/${repository}/actions/workflows/${encodedWorkflow}/runs?${params}`;
const runs = await collectGithubPages((page) => github(`${runsPath}&page=${page}`), "workflow_runs");
const candidates = [];

for (const run of runs) {
  const jobsPath = `/repos/${repository}/actions/runs/${run.id}/jobs?filter=latest&per_page=100`;
  const jobs = await collectGithubPages((page) => github(`${jobsPath}&page=${page}`), "jobs");
  candidates.push({ run, jobs });
}

const evidence = selectExactCiEvidence(sha, candidates);
console.log(
  `Exact-SHA full CI evidence OK: ${sha} via run ${evidence.run.id}`
    + `${evidence.run.html_url ? ` (${evidence.run.html_url})` : ""}.`
);
