import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

// The Rust `model` module is `crates/tine-core/src/model.rs` plus the seam
// files K3 cut out of it under `model/` (2026-09-15). A source guard that reads
// model.rs alone passes vacuously for code that moved, so every TS guard over
// the model module reads it through here (I-11). The Rust twins are
// `test_support::model_module_files` (in-crate tests) and
// `production_source::model_module_files` (integration tests).
const MODEL_RS = "crates/tine-core/src/model.rs";
const MODEL_DIR = "crates/tine-core/src/model";

function rsFilesUnder(dir: string): string[] {
  return readdirSync(join(process.cwd(), dir))
    .sort()
    .flatMap((name) => {
      const path = `${dir}/${name}`;
      if (statSync(join(process.cwd(), path)).isDirectory()) return rsFilesUnder(path);
      return name.endsWith(".rs") ? [path] : [];
    });
}

/** Every file of the model module, repository-relative, model.rs first. */
export function modelModuleFiles(): string[] {
  return [MODEL_RS, ...rsFilesUnder(MODEL_DIR)];
}

/** The whole model module's source: its files concatenated in that order. */
export function modelModuleSource(): string {
  return modelModuleFiles()
    .map((path) => readFileSync(join(process.cwd(), path), "utf8"))
    .join("\n");
}
