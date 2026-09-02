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

  it("keeps switcher resource identity and Settings busy ownership explicit", () => {
    const switcher = readFileSync(join(process.cwd(), "src/components/QuickSwitcher.tsx"), "utf8");
    const settings = readFileSync(join(process.cwd(), "src/components/Settings.tsx"), "utf8");
    expect(switcher).toContain("graphBinding: graphBinding()");
    expect(settings.match(/if \(busy\(\) === myKey\) setBusy\(null\)/g)?.length ?? 0).toBeGreaterThanOrEqual(4);
    expect(settings.match(/if \(props\.busy\(\) === myKey\) props\.setBusy\(null\)/g)?.length ?? 0).toBe(2);
  });
});
