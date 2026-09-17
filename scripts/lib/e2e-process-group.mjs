// Orphan detection for scenario process trees.
//
// Every native scenario is spawned detached, so its whole tree -- taskset,
// xvfb, dbus, the driver, the app and anything they start -- shares one process
// group whose id is the child's pid. A process still in that group after the
// scenario has exited is an orphan the suite created and did not reap.
//
// This is invisible in any single run: the `vite preview` orphan of v0.6.984
// was free the first time and fatal the fifth, so it only ever appeared during
// a release, presenting as "the machine ran out of memory". The failing input
// is concrete -- a journey that starts a server and exits without killing it --
// which is why this lives here with a test rather than inline in the runner.
import { spawnSync } from "node:child_process";

/** Processes still in group `pgid`, excluding this process and zombies. */
export function survivingProcessGroup(pgid, listProcesses = defaultProcessList) {
  if (process.platform === "win32") return [];
  return listProcesses()
    .map((line) => line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/))
    .filter((match) => match && Number(match[2]) === pgid && Number(match[1]) !== process.pid)
    // A reaped-but-unwaited child holds no memory and no port.
    .filter((match) => !match[3].includes("<defunct>"))
    .map((match) => ({ pid: Number(match[1]), args: match[3].slice(0, 200) }));
}

function defaultProcessList() {
  const listed = spawnSync("ps", ["-eo", "pid=,pgid=,args="], { encoding: "utf8" });
  if (listed.status !== 0 || typeof listed.stdout !== "string") return [];
  return listed.stdout.split("\n");
}

/**
 * Teardown is not instantaneous: WebKit, the driver and the bus can outlive the
 * scenario's exit by a beat, and killing them at that moment would report a leak
 * that does not exist. Poll for the group to empty, then kill whatever is left
 * so one leaking journey cannot starve the rest of the suite, and return the
 * survivors so the caller can name the journey that produced them.
 */
export async function reapProcessGroup(pgid, options = {}) {
  const { attempts = 10, intervalMs = 300, list, kill = defaultKill } = options;
  let survivors = [];
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    survivors = survivingProcessGroup(pgid, list);
    if (survivors.length === 0) return [];
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
  survivors = survivingProcessGroup(pgid, list);
  if (survivors.length === 0) return [];
  kill(pgid);
  return survivors;
}

function defaultKill(pgid) {
  try { process.kill(-pgid, "SIGKILL"); } catch {}
}
