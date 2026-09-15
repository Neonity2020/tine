import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";

const root = process.cwd();

function readImportedStylesheet(relative: string, seen: Set<string>): string {
  const normalized = relative.split(path.sep).join("/");
  if (seen.has(normalized)) throw new Error(`stylesheet import cycle at ${normalized}`);
  seen.add(normalized);
  const source = readFileSync(path.join(root, normalized), "utf8");
  const expanded = source.replace(
    /@import\s+(?:url\(\s*)?["']([^"']+)["']\s*\)?\s*;/g,
    (_statement, specifier: string) =>
      readImportedStylesheet(path.join(path.dirname(normalized), specifier), seen),
  );
  seen.delete(normalized);
  return expanded;
}

/** The complete application stylesheet, with imports expanded at their cascade position. */
export function readAppStylesheet(): string {
  return readImportedStylesheet("src/styles/app.css", new Set());
}

/** Block's implementation boundary: its public parent plus every extracted sibling module. */
export function readBlockModuleSource(): string {
  const directory = path.join(root, "src/components/block");
  const files = [
    "src/components/Block.tsx",
    ...readdirSync(directory)
      .filter((file) => /\.tsx?$/.test(file))
      .sort()
      .map((file) => `src/components/block/${file}`),
  ];
  return files.map((file) => `// ${file}\n${readFileSync(path.join(root, file), "utf8")}`).join("\n");
}

/** Store's implementation boundary: its public façade plus every extracted child module. */
export function readStoreModuleSource(): string {
  const directory = path.join(root, "src/store");
  const files = [
    "src/store.ts",
    ...readdirSync(directory)
      .filter((file) => /\.tsx?$/.test(file) && !/\.(test|spec)\.tsx?$/.test(file))
      .sort()
      .map((file) => `src/store/${file}`),
  ];
  return files.map((file) => `// ${file}\n${readFileSync(path.join(root, file), "utf8")}`).join("\n");
}
