import fs from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

// Native save uses atomic replacement. The caller's predicate is deliberately
// responsible for the exact bytes it needs to observe; this helper only owns
// bounded polling for that resulting file.
export async function waitForFileText(file, predicate, label, {
  timeoutMs = 10_000,
  intervalMs = 100,
  readFile = fs.readFileSync,
  now = Date.now,
  delay = sleep,
} = {}) {
  const deadline = now() + timeoutMs;
  let lastText;
  let sawTransientAbsence = false;
  while (now() < deadline) {
    let text;
    try {
      text = readFile(file, "utf8");
    } catch (error) {
      // A save publishes through rename, so an observer may hit the small
      // unlink-to-replacement window. Only that expected absence is retried;
      // permission, I/O, and malformed-path failures remain immediate errors.
      if (error?.code !== "ENOENT") throw error;
      sawTransientAbsence = true;
      await delay(intervalMs);
      continue;
    }
    lastText = text;
    if (predicate(text)) return text;
    await delay(intervalMs);
  }
  const observation = lastText ?? (sawTransientAbsence ? "[file unavailable after atomic replacement]" : "[file unavailable]");
  throw new Error(`${label} was not persisted: ${observation}`);
}
