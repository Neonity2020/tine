#!/usr/bin/env node

import fs from "node:fs";

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const runId = Number(option("--run-id"));
const repository = option("--repo") ?? process.env.GITHUB_REPOSITORY;
const expectedSource = (option("--source") ?? "").toLowerCase();
const output = option("--output");
const token = process.env.GH_TOKEN ?? process.env.GITHUB_TOKEN;
const apiBase = process.env.GITHUB_API_URL ?? "https://api.github.com";
if (!Number.isSafeInteger(runId) || runId <= 0 || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository || "")
  || !/^[0-9a-f]{40}$/.test(expectedSource) || !output || !token) {
  throw new Error("usage: check-release-promotion-source.mjs --run-id ID --repo OWNER/REPO --source SHA --output FILE (with GH_TOKEN)");
}

async function github(endpoint) {
  const response = await fetch(`${apiBase}${endpoint}`, {
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: `Bearer ${token}`,
      "X-GitHub-Api-Version": "2022-11-28",
    },
  });
  if (!response.ok) throw new Error(`GitHub API ${response.status} for ${endpoint}: ${await response.text()}`);
  return response.json();
}

const run = await github(`/repos/${repository}/actions/runs/${runId}`);
if (run.event !== "workflow_dispatch" || run.status !== "completed" || run.conclusion !== "success"
  || String(run.head_sha).toLowerCase() !== expectedSource || run.path !== ".github/workflows/release.yml") {
  throw new Error(`run ${runId} is not a successful manual release candidate for ${expectedSource}`);
}
const jobs = await github(`/repos/${repository}/actions/runs/${runId}/jobs?filter=latest&per_page=100`);
const conclusions = new Map((jobs.jobs ?? []).map((job) => [job.name, job.conclusion]));
for (const required of ["preflight", "assemble"]) {
  if (conclusions.get(required) !== "success") throw new Error(`source run ${runId} ${required} job did not succeed`);
}
if (conclusions.get("publish-release") !== "skipped") {
  throw new Error(`source run ${runId} was not a no-publication build run`);
}
const artifactsPayload = await github(`/repos/${repository}/actions/runs/${runId}/artifacts?per_page=100`);
const requiredArtifacts = [
  "release-candidate",
  "release-candidate-receipt",
  "release-proof-linux-x64",
  "release-proof-windows-x64",
];
const artifacts = [];
for (const name of requiredArtifacts) {
  const artifact = (artifactsPayload.artifacts ?? []).find((entry) => entry.name === name && !entry.expired);
  if (!artifact) throw new Error(`source run ${runId} lacks unexpired ${name}`);
  artifacts.push({ id: artifact.id, name, size: artifact.size_in_bytes, digest: artifact.digest ?? null });
}
const evidence = {
  schemaVersion: 1,
  kind: "tine-release-promotion-source",
  repository,
  runId,
  sourceCommit: expectedSource,
  url: run.html_url,
  artifacts,
};
fs.writeFileSync(output, `${JSON.stringify(evidence, null, 2)}\n`);
console.log(`Promotion source OK: run ${runId} at ${expectedSource}.`);
