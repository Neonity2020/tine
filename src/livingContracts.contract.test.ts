import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const source = (path: string) => readFileSync(join(process.cwd(), path), "utf8");

describe("living publication contracts stay pinned to production", () => {
  it("pins backup restore to its capability-bound retire-before-publish stack", () => {
    const contract = source("docs/contracts/backup-restore.md");
    const production = source("src-tauri/src/backup.rs");
    for (const name of [
      "move_live_to_recovery",
      "atomic_copy_new_into_live",
      "rename_noreplace_between",
    ]) {
      expect(contract, `backup restore contract must name ${name}`).toContain(name);
      expect(production, `backup restore production must retain ${name}`).toContain(`fn ${name}(`);
    }
    const restoreStart = production.indexOf("fn restore_scoped_graph_text_copy(");
    const restore = production.slice(
      restoreStart,
      production.indexOf("\nfn ", restoreStart + 4),
    );
    expect(
      restore.indexOf("move_live_to_recovery("),
      "each existing live file must be retired before its snapshot bytes publish",
    ).toBeLessThan(restore.indexOf("atomic_copy_new_into_live("));
  });

  it("pins both user-selected report exporters to atomic_write", () => {
    const contract = source("docs/contracts/report-exports.md");
    const diagnostic = source("src-tauri/src/debug.rs");
    const verification = source("src-tauri/src/graph_verification.rs");
    expect(contract).toContain("save_diagnostic_report");
    expect(contract).toContain("tine_core::model::atomic_write");
    expect(diagnostic).toContain("pub(crate) async fn save_diagnostic_report(");
    expect(diagnostic).toContain("tine_core::model::atomic_write(&path");
    expect(verification).toContain("tine_core::model::atomic_write(&path");
  });

  it("pins the small-file conditional publish, recovery name, and retry bound", () => {
    const contract = source("docs/contracts/small-file-writes.md");
    const production = source("crates/tine-core/src/model.rs");
    for (const claim of [
      "atomic_replace_expected",
      "Graph::recover_interrupted_publishes()",
      'RETIRED_SUFFIX = ".retired"',
      "four-attempt",
    ]) {
      expect(contract, `small-file contract must retain load-bearing claim ${claim}`).toContain(claim);
    }
    expect(production).toContain("pub fn recover_interrupted_publishes(&self)");
    expect(production).toContain('const RETIRED_SUFFIX: &str = ".retired"');
    expect(production).toContain("match atomic_replace_expected(path, current.as_bytes(), next.as_bytes())?");
    expect(production).toContain("for attempt in 0..4");
  });
});
