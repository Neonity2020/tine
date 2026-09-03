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
  {
    file: "persistence.ts",
    line: 571,
    class: "temporary-direct-save-parser",
    why: "retired by W4-E4",
  },
  {
    file: "persistence.ts",
    line: 592,
    class: "temporary-direct-save-parser",
    why: "retired by W4-E4",
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

    const lines = text.split("\n");
    const functions: { name: string; start: number; end: number; parameter: string | null }[] = [];
    for (let index = 0; index < lines.length; index += 1) {
      const signature = lines[index].match(
        /(?:function\s+([A-Za-z_$][\w$]*)|(?:const|let)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?)\s*\([^)]*\)\s*(?::[^={]+)?(?:=>\s*)?\{/,
      );
      if (!signature) continue;
      const parameter = lines[index].match(/\b(message|text)\s*:\s*string\b/)?.[1] ?? null;
      let depth = 0;
      let end = index;
      for (; end < lines.length; end += 1) {
        depth += (lines[end].match(/\{/g) ?? []).length;
        depth -= (lines[end].match(/\}/g) ?? []).length;
        if (depth === 0) break;
      }
      functions.push({ name: signature[1] ?? signature[2], start: index, end, parameter });
      index = end;
    }

    const helperNames = new Set<string>();
    for (const fn of functions) {
      if (!fn.parameter || !/(?:error|failure|classif)/i.test(fn.name)) continue;
      const aliases = new Set([fn.parameter]);
      let firstDerivedInput: number | null = null;
      let classifierLine: number | null = null;
      for (let index = fn.start + 1; index <= fn.end; index += 1) {
        const line = lines[index];
        const derived = line.match(/\b(?:const|let)\s+([A-Za-z_$][\w$]*)\s*=\s*([A-Za-z_$][\w$]*)\.(?:slice|substring|replace|trim)\b/);
        if (derived && aliases.has(derived[2])) {
          aliases.add(derived[1]);
          firstDerivedInput ??= index;
        }
        const classifiesAlias = [...aliases].some((alias) =>
          new RegExp(`\\b${alias}\\.(?:includes|match|startsWith)\\(`).test(line)
          || new RegExp(`\\.(?:exec|test)\\(${alias}\\)`).test(line),
        );
        if (classifiesAlias) {
          classifierLine = firstDerivedInput ?? index;
          break;
        }
      }
      if (classifierLine !== null) {
        helperNames.add(fn.name);
        sites.push({
          file: relative(ROOT, file).replaceAll("\\", "/"),
          line: classifierLine + 1,
        });
      }
    }

    for (const fn of functions) {
      if (fn.parameter) continue;
      const body = lines.slice(fn.start, fn.end + 1).join("\n");
      const convertsErrorToText = /\b(?:message|text)\s*=\s*String\((?:error|err|e)\)/.test(body);
      const delegatesToHelper = [...helperNames].some((name) =>
        new RegExp(`\\b${name}\\((?:message|text)\\)`).test(body),
      );
      if (convertsErrorToText && delegatesToHelper) {
        sites.push({ file: relative(ROOT, file).replaceAll("\\", "/"), line: fn.start + 1 });
      }
    }
  }
  return sites
    .filter((site, index, all) =>
      all.findIndex((candidate) => candidate.file === site.file && candidate.line === site.line) === index,
    )
    .sort((left, right) => left.file.localeCompare(right.file) || left.line - right.line);
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

    // The one kind with a typed detail object: bounded counts and paths are
    // validated field by field; a malformed detail degrades to no detail.
    const mismatch = classifyNativeCallError(JSON.stringify({
      kind: "shared-frontier-mismatch",
      detail: {
        local_pages: 2, shared_pages: 2, local_only: 1, shared_only: 0, changed: 1, omitted: 0,
        paths: [
          { path: "notes/local-only.md", side: "local-only" },
          { path: "notes/changed.md", side: "changed", categories: ["outline"] },
        ],
      },
    }));
    expect(mismatch).toBeInstanceOf(SharedFrontierMismatchError);
    expect(mismatch).toMatchObject({
      detail: {
        localOnly: 1,
        paths: [
          { path: "notes/local-only.md", side: "local-only", categories: [] },
          { path: "notes/changed.md", side: "changed", categories: ["outline"] },
        ],
      },
    });
    const malformed = classifyNativeCallError(JSON.stringify({
      kind: "shared-frontier-mismatch",
      detail: { local_pages: 1, paths: [{ path: 7, side: "elsewhere" }] },
    }));
    expect(malformed).toBeInstanceOf(SharedFrontierMismatchError);
    expect(malformed).toMatchObject({ detail: null });
  });

  it("has no prose-parsing classifier outside the one funnel", () => {
    expect(
      errorStringClassifierSites(),
      "I-9: error classification must use src/backend.ts SaveConflictError/classifyTaggedBackendError; helper indirection may not restore prose parsing",
    ).toEqual(
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
