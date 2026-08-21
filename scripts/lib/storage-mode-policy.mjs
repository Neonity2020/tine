export function evaluateStorageMode(policy, report) {
  const failures = [];
  const lines = [];
  const storage = policy.storageMode;
  if (!storage || !Array.isArray(storage.modes) || !storage.operations) {
    failures.push("policy does not define the storage-mode report contract");
  }
  if (report.schemaVersion !== 2 || report.kind !== "storage-mode") {
    failures.push("expected a schema-2 storage-mode measurement");
  }
  if (!report.manifest?.fixture?.name || !report.manifest?.fixture?.graph) {
    failures.push("storage-mode report is missing exact fixture provenance");
  }
  const minTextFiles = storage?.requiredFixture?.minTextFiles;
  const fixtureFiles = report.manifest?.fixture?.fileCount;
  if (!Number.isInteger(minTextFiles) || minTextFiles <= 0) {
    failures.push("policy does not define a positive storage-mode real-corpus file floor");
  } else if (!Number.isInteger(fixtureFiles) || fixtureFiles < minTextFiles) {
    failures.push(`storage-mode fixture has ${fixtureFiles ?? "unknown"} text files; policy requires at least ${minTextFiles}`);
  }
  if (!Array.isArray(report.rounds) || report.rounds.length < policy.reliability?.rounds) {
    failures.push(`storage-mode report has fewer than ${policy.reliability?.rounds ?? "the required"} rounds`);
  }
  const operations = Object.keys(storage?.operations ?? {});
  lines.push("operation                 direct ms  managed ms  managed delta  direct spread  managed spread");
  for (const name of operations) {
    const direct = report.modes?.direct?.metrics?.[name];
    const managed = report.modes?.managed?.metrics?.[name];
    const directValue = direct?.rawMedianOfRoundMins;
    const managedValue = managed?.rawMedianOfRoundMins;
    const directSpread = direct?.roundSpreadPct;
    const managedSpread = managed?.roundSpreadPct;
    if (![directValue, managedValue, directSpread, managedSpread].every(Number.isFinite)) {
      failures.push(`${name}: missing paired direct/managed measurement or round spread`);
      continue;
    }
    const delta = ((managedValue / directValue) - 1) * 100;
    lines.push(
      `${(storage.operations[name].label ?? name).padEnd(25)} ` +
      `${directValue.toFixed(1).padStart(9)}  ${managedValue.toFixed(1).padStart(10)}  ` +
      `${`${delta.toFixed(1)}%`.padStart(13)}  ${`${directSpread.toFixed(1)}%`.padStart(13)}  ` +
      `${`${managedSpread.toFixed(1)}%`.padStart(14)}`,
    );
    const budget = storage.operations[name];
    if (!Number.isFinite(budget.managedMaxMs) || budget.managedMaxMs <= 0) {
      failures.push(`${name}: policy is missing a positive managedMaxMs`);
    } else if (managedValue > budget.managedMaxMs) {
      failures.push(`${name}: managed ${managedValue.toFixed(1)} ms exceeds ${budget.managedMaxMs} ms`);
    }
    if (budget.managedMaxDeltaPct !== undefined) {
      if (!Number.isFinite(budget.managedMaxDeltaPct) || budget.managedMaxDeltaPct < 0) {
        failures.push(`${name}: policy has an invalid managedMaxDeltaPct`);
      } else if (delta > budget.managedMaxDeltaPct) {
        failures.push(`${name}: managed delta ${delta.toFixed(1)}% exceeds ${budget.managedMaxDeltaPct}%`);
      }
    }
    if (budget.maxRoundSpreadPct !== undefined) {
      if (!Number.isFinite(budget.maxRoundSpreadPct) || budget.maxRoundSpreadPct < 0) {
        failures.push(`${name}: policy has an invalid maxRoundSpreadPct`);
      } else {
        if (directSpread > budget.maxRoundSpreadPct) {
          failures.push(`${name}: Direct round spread ${directSpread.toFixed(1)}% exceeds ${budget.maxRoundSpreadPct}%`);
        }
        if (managedSpread > budget.maxRoundSpreadPct) {
          failures.push(`${name}: managed round spread ${managedSpread.toFixed(1)}% exceeds ${budget.maxRoundSpreadPct}%`);
        }
      }
    }
  }
  return { failures, lines };
}
