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

  // Named by behaviour, not by shell variable: the probe was refactored into a
  // per-device smoke_one() function for GH #446 (an iPad has to boot too), and
  // an assertion on the old UPPERCASE variable names would have failed a
  // refactor that changed nothing this test cares about.
  it("exercises Guide copy inside the iOS Simulator rather than only launching the app", () => {
    const workflow = readFileSync(".github/workflows/ios-probe.yml", "utf8");
    // The launch asks the app to copy the Guide in, and the assertion afterwards
    // proves a non-empty Guide page landed in the container graph.
    expect(workflow).toContain("--tine-ci-copy-guide");
    expect(workflow).toMatch(/guide_page="?\$\(find .*-name '\*Tine Guide.md'/i);
    expect(workflow).toMatch(/test -s "\$guide_page"/i);
    // The launch argument is a container path whose Application UUID is stale,
    // so the probe also proves the app rebases it instead of trusting it.
    expect(workflow).toMatch(/stale_graph/i);
    expect(workflow).toMatch(/00000000-0000-4000-8000-000000000000/);
  });

  it("carries an iOS-rebased graph path across the Rust mobile-plugin bridge", () => {
    const bridge = readFileSync("src-tauri/src/ios_folder_picker.rs", "utf8");
    expect(bridge).toMatch(
      /struct PrepareGraphFolderResult\s*\{[\s\S]*?path:\s*Option<String>/
    );
  });
});
