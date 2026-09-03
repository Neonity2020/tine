import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const swept = [
  "src/components/QuickSwitcher.tsx",
  "src/router.ts",
  "src/components/Block.tsx",
  "src/plugins/ownership.ts",
  "src/reloadOnFocus.ts",
  "src/components/Settings.tsx",
];

describe("I-20 graph identity guard", () => {
  it("never compares staleness with the render epoch at a swept landing site", () => {
    const offenders = swept.filter((file) =>
      readFileSync(join(process.cwd(), file), "utf8").includes("graphEpoch()"),
    );
    expect(
      offenders,
      "I-20: graph identity is graphBindingRev, never graphEpoch(); see persistence.ts:362 and the Harvest D dossier",
    ).toEqual([]);
  });

  it("keeps switcher resource identity and forbids unowned Settings busy clears", () => {
    const switcher = readFileSync(join(process.cwd(), "src/components/QuickSwitcher.tsx"), "utf8");
    const settings = readFileSync(join(process.cwd(), "src/components/Settings.tsx"), "utf8");
    expect(switcher).toContain("graphBinding: graphBinding()");

    const pluginsStart = settings.indexOf("function PluginsTab(): JSX.Element");
    const pluginsTab = settings.slice(
      pluginsStart,
      settings.indexOf("function OgField(", pluginsStart),
    );
    const normalized = pluginsTab.replace(/\s+/g, " ");
    const ownedClears = [...normalized.matchAll(/if \(busy\(\) === myKey\) setBusy\(null\)/g)];
    const busySets = [...normalized.matchAll(/setBusy\(myKey\)/g)];
    expect(
      ownedClears.length,
      "I-20: Settings PluginsTab must retain its five identity-owned busy clears; imitate managedStorageRuntime.ts",
    ).toBe(5);
    expect(
      ownedClears.length,
      "I-20: every Settings PluginsTab busy-setting operation must have an identity-owned clear; imitate managedStorageRuntime.ts",
    ).toBeGreaterThanOrEqual(busySets.length);
    const unownedClears = [...normalized.matchAll(/(?:props\.)?setBusy\(null\)/g)]
      .filter((match) => {
        const prefix = normalized.slice(Math.max(0, match.index! - 96), match.index);
        return !/if \((?:props\.)?busy\(\) === myKey\) (?:\{ )?$/.test(prefix);
      })
      .map((match) => match.index);
    expect(
      unownedClears,
      "I-20: a completed Settings operation may clear only the busy token it owns",
    ).toEqual([]);
  });
});
