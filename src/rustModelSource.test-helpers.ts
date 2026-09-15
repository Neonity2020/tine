import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

// A split Rust module lives in `X.rs` plus the files under `X/`. K3 split
// `model` (2026-09-15), and K7 split `query`, `publish`, `watcher` and
// `commands` the same way. A source guard that reads `X.rs` alone passes
// vacuously for code that moved, so every TS guard over such a module reads it
// through here (I-11). The Rust twins are:
// - `test_support::model_module_files` and
//   `projection_producer_census::production_rust` for in-crate tests;
// - `production_source::module_files` and `module_source` for integration tests;
// - src-tauri's `test_support::rust_module_source` and
//   `rust_module_production_source`.
const MODEL_RS = "crates/tine-core/src/model.rs";

/** Every `.rs` under `dir`, sorted, except `*_tests.rs` test bodies. A test body
 *  is the file behind `#[cfg(test)] #[path = "X_tests.rs"] mod tests;`. */
function rsFilesUnder(dir: string): string[] {
  return readdirSync(join(process.cwd(), dir))
    .sort()
    .flatMap((name) => {
      const path = `${dir}/${name}`;
      if (statSync(join(process.cwd(), path)).isDirectory()) return rsFilesUnder(path);
      return name.endsWith(".rs") && !name.endsWith("_tests.rs") ? [path] : [];
    });
}

/** Every production file of the Rust module whose root file is `root`, for
 *  example `"crates/tine-core/src/query.rs"`. Paths are repository-relative:
 *  the root first, then everything under its sibling directory, if it has one. */
export function rustModuleFiles(root: string): string[] {
  const dir = root.replace(/\.rs$/, "");
  const full = join(process.cwd(), dir);
  return [root, ...(existsSync(full) && statSync(full).isDirectory() ? rsFilesUnder(dir) : [])];
}

/** The whole module's source: its files concatenated in `rustModuleFiles` order. */
export function rustModuleSource(root: string): string {
  return rustModuleFiles(root)
    .map((path) => readFileSync(join(process.cwd(), path), "utf8"))
    .join("\n");
}

/** Every file of the model module, repository-relative, model.rs first. */
export function modelModuleFiles(): string[] {
  return rustModuleFiles(MODEL_RS);
}

/** The whole model module's source: its files concatenated in that order. */
export function modelModuleSource(): string {
  return rustModuleSource(MODEL_RS);
}
