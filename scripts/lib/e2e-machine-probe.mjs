// A failed wait-for-external-change cannot tell you WHICH of two things
// happened, and the suite never asked.
//
// On 2026-09-17 five external-change journeys failed together for forty
// minutes across three commits and two builds, then healed with no code
// change. Each said the same thing: the app did not show the external edit
// within 10s. That sentence is compatible with a product regression and with a
// machine whose filesystem had stopped answering in tens of milliseconds --
// another agent was deleting ~90GB of worktrees from the same overlay mount --
// and the capsule contained nothing that separated them. The forty minutes
// went on reverting a suspected production term (it was innocent), on
// suspecting the merge (innocent), and finally on re-running a commit that had
// been green an hour earlier, which failed too.
//
// The 10s timeout is an unverified claim about I/O latency. This probe
// measures the claim at the moment it is used, so the capsule says either "the
// filesystem answered in 4ms, so this is the product" or "writes were taking
// seconds; the machine was starved". It runs ONLY on the failure path: it can
// slow down a red run, never a green one.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * Time the exact operation the external-change journeys wait on: write a small
 * file, fsync it, rename it into place, and read it back. Returns milliseconds
 * per full cycle.
 */
export function measureFilesystemLatency(directory, samples = 15) {
  const timings = [];
  const probe = path.join(directory, `.e2e-machine-probe-${process.pid}`);
  try {
    for (let index = 0; index < samples; index += 1) {
      const started = process.hrtime.bigint();
      const handle = fs.openSync(`${probe}.tmp`, "w");
      fs.writeSync(handle, `${index}\n`);
      fs.fsyncSync(handle);
      fs.closeSync(handle);
      fs.renameSync(`${probe}.tmp`, probe);
      fs.readFileSync(probe, "utf8");
      timings.push(Number(process.hrtime.bigint() - started) / 1e6);
    }
  } catch (error) {
    return { samples: timings.length, error: String(error?.message ?? error) };
  } finally {
    for (const file of [`${probe}.tmp`, probe]) {
      try { fs.rmSync(file, { force: true }); } catch { /* the probe never fails a run */ }
    }
  }
  const sorted = [...timings].sort((left, right) => left - right);
  return {
    samples: sorted.length,
    medianMs: Number(sorted[Math.floor(sorted.length / 2)].toFixed(3)),
    maxMs: Number(sorted[sorted.length - 1].toFixed(3)),
  };
}

/**
 * Both mounts a journey depends on: the artifact/checkout filesystem and the
 * one its temporary graph lives on. They are frequently different devices, and
 * only one of them has to stall for a wait to expire.
 */
export function machineSnapshot(directory) {
  const [load1] = os.loadavg();
  return {
    filesystem: measureFilesystemLatency(directory),
    tmpFilesystem: measureFilesystemLatency(os.tmpdir()),
    loadAverage1: Number(load1.toFixed(2)),
    cpus: os.cpus().length,
    freeMemoryMb: Math.round(os.freemem() / 1024 / 1024),
  };
}

/**
 * The sentence the capsule owes its reader. Thresholds are deliberately far
 * from normal: a warm write-fsync-rename-read cycle on this machine is single
 * -digit milliseconds, and the journeys wait 10s.
 */
export function describeMachine(snapshot) {
  const { loadAverage1, cpus } = snapshot;
  const probes = [snapshot.filesystem, snapshot.tmpFilesystem].filter(Boolean);
  const broken = probes.find((probe) => probe.error);
  if (broken) return `the filesystem probe itself failed (${broken.error}); machine state is unknown`;
  // The slower mount is the one that decides whether a wait expires.
  const filesystem = probes.reduce((worst, probe) => (probe.medianMs > worst.medianMs ? probe : worst));
  const starvedDisk = filesystem.medianMs >= 100 || filesystem.maxMs >= 1000;
  const starvedCpu = loadAverage1 >= cpus * 2;
  if (starvedDisk || starvedCpu) {
    return `the machine was starved while this failed (write-fsync-rename-read median ${filesystem.medianMs}ms, `
      + `max ${filesystem.maxMs}ms; load ${loadAverage1} on ${cpus} cpus) -- re-run before reading the diff`;
  }
  return `the filesystem answered in ${filesystem.medianMs}ms (max ${filesystem.maxMs}ms) and load was ${loadAverage1} `
    + `on ${cpus} cpus, so a missed 10s wait is the product, not the machine`;
}
