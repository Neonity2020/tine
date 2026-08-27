import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const sources = [
  "crates/tine-core/src/model.rs",
  "crates/tine-core/src/oplog/enrollment.rs",
  "crates/tine-core/src/oplog/projection_store.rs",
];

describe("iOS atomic publication platform boundary", () => {
  it("routes every Darwin renameatx publication through the iOS implementation", () => {
    for (const path of sources) {
      const source = readFileSync(path, "utf8");
      expect(source, path).not.toContain('#[cfg(target_os = "macos")]');
    }
  });

  it("admits iOS wherever the graph projection platform is selected", () => {
    const model = readFileSync("crates/tine-core/src/model.rs", "utf8");
    const platformGate = model.slice(
      model.indexOf("fn require_projection_platform()"),
      model.indexOf("fn open_projection_root_nofollow")
    );
    expect(platformGate).toContain('target_os = "ios"');
  });

  it("exercises Guide copy inside the iOS Simulator rather than only launching the app", () => {
    const workflow = readFileSync(".github/workflows/ios-probe.yml", "utf8");
    expect(workflow).toContain("--tine-ci-copy-guide");
    expect(workflow).toContain("GUIDE_PAGE");
    expect(workflow).toContain("'*Tine Guide.md'");
    expect(workflow).toContain("STALE_GRAPH");
  });
});
