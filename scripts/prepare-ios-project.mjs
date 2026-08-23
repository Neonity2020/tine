import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const root = process.cwd();
const source = path.join(root, "src-tauri", "Tine.ios.entitlements");
const generatedRoot = path.join(root, "src-tauri", "gen", "apple");

if (!fs.existsSync(source)) {
  throw new Error(`missing tracked iOS entitlements: ${source}`);
}
if (!fs.existsSync(generatedRoot)) {
  throw new Error("the generated iOS project is absent; run `npx tauri ios init --ci` first");
}

const generated = [];
for (const entry of fs.readdirSync(generatedRoot, { withFileTypes: true })) {
  if (!entry.isDirectory() || !entry.name.endsWith("_iOS")) continue;
  const directory = path.join(generatedRoot, entry.name);
  for (const child of fs.readdirSync(directory, { withFileTypes: true })) {
    if (child.isFile() && child.name.endsWith("_iOS.entitlements")) {
      generated.push(path.join(directory, child.name));
    }
  }
}

if (generated.length !== 1) {
  throw new Error(`expected exactly one generated iOS entitlements file, found ${generated.length}`);
}

fs.copyFileSync(source, generated[0]);
console.log(`installed iCloud entitlements at ${path.relative(root, generated[0])}`);
