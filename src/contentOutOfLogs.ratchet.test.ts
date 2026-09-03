import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { parserDiagnostic } from "./devtools/lsdoc-diff/diagnostic";

const source = (path: string) => readFileSync(join(process.cwd(), path), "utf8");
const SRC = fileURLToPath(new URL(".", import.meta.url));

interface ConsoleSite {
  file: string;
  line: number;
  method: "log" | "warn" | "error" | "debug";
}

const CONSOLE_ALLOWLIST_SIZE = 21;
const CONSOLE_ALLOWLIST: readonly (ConsoleSite & { class: string; why: string })[] = [
  { file: "App.tsx", line: 1071, method: "warn", class: "local-error", why: "native listener setup error stays in uncaptured WebView devtools" },
  { file: "capture.tsx", line: 173, method: "log", class: "numeric-shape", why: "capture-window sizing measurements contain only numbers" },
  { file: "capture.tsx", line: 600, method: "error", class: "local-error", why: "parser bootstrap error stays in uncaptured WebView devtools" },
  { file: "components/Block.tsx", line: 1509, method: "warn", class: "local-error", why: "optional autocomplete failure stays in uncaptured WebView devtools" },
  { file: "logbook.ts", line: 42, method: "error", class: "local-error", why: "marker transition failure stays in uncaptured WebView devtools" },
  { file: "main.tsx", line: 62, method: "error", class: "local-error", why: "window reveal failure stays in uncaptured WebView devtools" },
  { file: "main.tsx", line: 70, method: "error", class: "local-error", why: "parser bootstrap error stays in uncaptured WebView devtools" },
  { file: "pdfRenderCoordinator.ts", line: 341, method: "error", class: "local-error", why: "PDF renderer failure stays in uncaptured WebView devtools" },
  { file: "persistence.ts", line: 998, method: "warn", class: "numeric-shape", why: "save refusal carries only a count" },
  { file: "persistence.ts", line: 1003, method: "error", class: "numeric-shape", why: "save refusal carries only a count" },
  { file: "persistence.ts", line: 1151, method: "error", class: "local-error", why: "managed conflict capture failure stays in uncaptured WebView devtools" },
  { file: "print.ts", line: 97, method: "error", class: "local-error", why: "optional local renderer failure stays in uncaptured WebView devtools" },
  { file: "print.ts", line: 112, method: "error", class: "local-error", why: "print preparation failure stays in uncaptured WebView devtools" },
  { file: "print.ts", line: 156, method: "error", class: "local-error", why: "iframe print failure stays in uncaptured WebView devtools" },
  { file: "render/parse.ts", line: 51, method: "warn", class: "build-token", why: "compares two public parser build tags" },
  { file: "sheet/formulaEval.ts", line: 193, method: "warn", class: "internal-id-count", why: "performance warning carries an internal owner id and numeric count" },
  { file: "store.ts", line: 6936, method: "warn", class: "local-error", why: "post-commit cleanup failure stays in uncaptured WebView devtools" },
  { file: "ui.ts", line: 490, method: "error", class: "local-error", why: "capsule persistence failure stays in uncaptured WebView devtools" },
  { file: "ui.ts", line: 515, method: "error", class: "local-error", why: "capsule refresh failure stays in uncaptured WebView devtools" },
  { file: "ui.ts", line: 552, method: "error", class: "local-error", why: "capsule retirement failure stays in uncaptured WebView devtools" },
  { file: "update.ts", line: 148, method: "error", class: "scrubbed-error", why: "safeUpdaterErrorChain permits only classified updater stages and causes" },
];

function sourceFiles(dir: string, files: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      sourceFiles(full, files);
    } else if (/\.tsx?$/.test(entry) && !/\.test\.tsx?$/.test(entry) && !/^testSetup\./.test(entry)) {
      files.push(full);
    }
  }
  return files;
}

function closingParen(text: string, open: number): number {
  let depth = 1;
  let quote: "\"" | "'" | "`" | null = null;
  let escaped = false;
  for (let index = open + 1; index < text.length; index += 1) {
    const char = text[index];
    if (quote) {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === quote) quote = null;
      continue;
    }
    if (char === "\"" || char === "'" || char === "`") quote = char;
    else if (char === "(") depth += 1;
    else if (char === ")" && --depth === 0) return index;
  }
  throw new Error("unterminated console call");
}

function isFixedLiteral(argumentsText: string): boolean {
  return /^\s*(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\$]|\\.|\$(?!\{))*`)\s*,?\s*$/s.test(argumentsText);
}

function variableConsoleSites(): ConsoleSite[] {
  const sites: ConsoleSite[] = [];
  for (const file of sourceFiles(SRC)) {
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(/\bconsole\.(log|warn|error|debug)\s*\(/g)) {
      const open = match.index! + match[0].lastIndexOf("(");
      const end = closingParen(text, open);
      if (isFixedLiteral(text.slice(open + 1, end))) continue;
      sites.push({
        file: relative(SRC, file).replaceAll("\\", "/"),
        line: text.slice(0, match.index).split("\n").length,
        method: match[1] as ConsoleSite["method"],
      });
    }
  }
  return sites.sort((left, right) => left.file.localeCompare(right.file) || left.line - right.line);
}

describe("I-5 content-out-of-logs ratchet", () => {
  it("equals the reviewed production console census", () => {
    expect(CONSOLE_ALLOWLIST).toHaveLength(CONSOLE_ALLOWLIST_SIZE);
    for (const entry of CONSOLE_ALLOWLIST) {
      expect(entry.class, `${entry.file}:${entry.line} needs a class`).not.toBe("");
      expect(entry.why, `${entry.file}:${entry.line} needs a reason`).not.toBe("");
    }
    expect(
      variableConsoleSites(),
      "I-5: the variable-bearing console census changed. Log a count or a fixed string, never user content "
        + "(exemplar: persistence.ts logs `{ count }`); if a new site is legitimately content-free, add it to "
        + "CONSOLE_ALLOWLIST with its class and reason and bump CONSOLE_ALLOWLIST_SIZE",
    ).toEqual(
      CONSOLE_ALLOWLIST.map(({ file, line, method }) => ({ file, line, method })),
    );
  });

  it("pins the diagnostics contract to both allowlist sizes and gates", () => {
    const contract = source("docs/contracts/diagnostics.md");
    const rustRatchet = source("crates/tine-core/tests/content_out_of_logs.rs");
    expect(contract).toContain("74 Rust production print sites");
    expect(contract).toContain("21 variable-bearing frontend console sites");
    expect(contract).toContain("debug_enabled()");
    expect(contract).toContain("runtime_debug_diagnostics_enabled()");
    expect(rustRatchet).toContain("const RUST_PRINT_SITE_COUNT: usize = 74;");
  });

  it("makes parser failures fixed-shape before they cross the lsdoc-diff worker boundary", () => {
    const worker = source("src/devtools/lsdoc-diff/worker.ts");
    const client = source("src/devtools/lsdoc-diff/mldoc-client.ts");
    const orchestrator = source("src/devtools/lsdoc-diff/orchestrator.ts");

    for (const text of [worker, client, orchestrator]) {
      expect(text).not.toMatch(/detail:\s*String\s*\(/);
      expect(text).not.toMatch(/detail:\s*m\.detail\b/);
    }
    expect(worker).not.toMatch(/loadError\s*=\s*`[^`]*\$\{/);
    expect(worker).toContain("diagnostic:");
    expect(client).toContain("diagnostic:");
    expect(orchestrator).toContain("diagnostic:");
  });

  it("represents parser input only by offset, byte length, and an opaque hash", () => {
    const first = parserDiagnostic("private parser input");
    const second = parserDiagnostic("different parser input", 7);
    expect(Object.keys(first).sort()).toEqual(["inputBytes", "inputHash", "offset"]);
    expect(first.offset).toBeNull();
    expect(second.offset).toBe(7);
    expect(first.inputBytes).toBe(new TextEncoder().encode("private parser input").length);
    expect(first.inputHash).toMatch(/^[0-9a-f]{16}$/);
    expect(second.inputHash).not.toBe(first.inputHash);
  });
});
