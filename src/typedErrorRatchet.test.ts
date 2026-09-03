import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  AdoptionArchivedError,
  AssetTooLargeError,
  BackendError,
  ManagedActorRefusalError,
  ManagedGraphMismatchError,
  OperationCancelledError,
  SharedFrontierMismatchError,
  SparseShutdownRefusedError,
  SyncDataUnavailableError,
  classifyNativeCallError,
} from "./backend";

const ROOT = fileURLToPath(new URL(".", import.meta.url));
const source = (path: string) => readFileSync(join(process.cwd(), path), "utf8");

interface ClassifierSite {
  file: string;
  line: number;
  class: string;
  why: string;
}

const ERROR_STRING_CLASSIFIER_ALLOWLIST: readonly ClassifierSite[] = [
  {
    file: "components/Macro.tsx",
    line: 414,
    class: "bounded-result-code",
    why: "the query boundary's result-too-large prefix is a bounded wire code, not prose",
  },
  {
    file: "lib/referenceLoadError.ts",
    line: 26,
    class: "bounded-result-code",
    why: "the references boundary's result-too-large prefix is a bounded wire code, not prose",
  },
];

function sourceFiles(dir: string, files: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      if (full.endsWith(join("lsdoc-diff", "vendor"))) continue;
      sourceFiles(full, files);
    } else if (/\.tsx?$/.test(entry) && !/\.test\.tsx?$/.test(entry) && entry !== "backend.ts") {
      files.push(full);
    }
  }
  return files;
}

function errorStringClassifierSites(): Omit<ClassifierSite, "class" | "why">[] {
  const sites: Omit<ClassifierSite, "class" | "why">[] = [];
  for (const file of sourceFiles(ROOT)) {
    const text = readFileSync(file, "utf8");
    for (const [index, line] of text.split("\n").entries()) {
      if (
        /String\((?:error|err|e)\).*\.(?:includes|match|startsWith)\(/.test(line)
        || /\b(?:detail|message)\.(?:includes|match|startsWith)\(/.test(line)
        || /\.(?:exec|test)\((?:message|detail)\)/.test(line)
      ) {
        sites.push({ file: relative(ROOT, file).replaceAll("\\", "/"), line: index + 1 });
      }
    }
  }
  return sites.sort((left, right) => left.file.localeCompare(right.file) || left.line - right.line);
}

describe("I-9/I-11 typed backend error boundary", () => {
  it("classifies every tagged native payload once at the backend funnel", () => {
    const cases: [string, new (...args: never[]) => BackendError][] = [
      ["sync-data-unavailable", SyncDataUnavailableError],
      ["managed-graph-mismatch", ManagedGraphMismatchError],
      ["shared-frontier-mismatch", SharedFrontierMismatchError],
      ["adoption-archived", AdoptionArchivedError],
      ["sparse-shutdown-refused", SparseShutdownRefusedError],
      ["asset-too-large", AssetTooLargeError],
      ["operation-cancelled", OperationCancelledError],
    ];
    for (const [kind, Type] of cases) {
      const classified = classifyNativeCallError(JSON.stringify({ kind }));
      expect(classified).toBeInstanceOf(Type);
    }
    const actor = classifyNativeCallError(JSON.stringify({
      kind: "managed-actor-refusal",
      reason_code: "trusted_local.append_outcome_unknown",
    }));
    expect(actor).toBeInstanceOf(ManagedActorRefusalError);
    expect(actor).toMatchObject({ reasonCode: "trusted_local.append_outcome_unknown" });
  });

  it("has no prose-parsing classifier outside the one funnel", () => {
    expect(errorStringClassifierSites()).toEqual(
      ERROR_STRING_CLASSIFIER_ALLOWLIST.map(({ file, line }) => ({ file, line })),
    );
  });

  it("pins the Rust typed boundaries and the living contract", () => {
    const wire = source("crates/tine-core/src/oplog/wire.rs");
    const runtime = source("crates/tine-core/src/sync_runtime.rs");
    const contract = source("docs/contracts/typed-errors.md");
    expect(wire).toContain("Io(std::io::ErrorKind)");
    expect(wire).not.toMatch(/ScenarioError::Io\([^)]*(?:to_string|format!)/s);
    // Item 3 checkpoint: freeze the compile-probed census rather than let
    // the pre-existing stringification debt expand while its public typed
    // carrier is split into a later packet.
    expect(runtime.match(/map_err\(display\)/g) ?? []).toHaveLength(89);
    expect(runtime.match(/fn display\(/g) ?? []).toHaveLength(1);
    expect(contract).toContain("9 BackendError subclasses");
    expect(contract).toContain("item 3 checkpoint");
    expect(contract).toContain("TauriBackend.call");
  });
});
