import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const SEARCH_PACKAGE = "tine-search";
const WASM_LOCK = "crates/lsdoc-wasm/Cargo.lock";
const NATIVE_LOCK = "Cargo.lock";
const VENDORED_BYTES = "src/render/wasm/lsdoc_wasm_bytes.ts";
const WASM_SIZE_CEILING = "scripts/wasm-size-ceiling.json";

function field(block, name) {
  return block.match(new RegExp(`^${name} = "([^"]*)"$`, "m"))?.[1];
}

function packageIdentity(pkg) {
  return `${pkg.name} ${pkg.version}${pkg.source ? ` (${pkg.source})` : ""}`;
}

function dependencyReference(value, context) {
  const match = value.match(/^(\S+)(?:\s+(\S+?)(?:\s+\((.+)\))?)?$/);
  if (!match) throw new Error(`${context} has malformed dependency reference ${JSON.stringify(value)}`);
  return { name: match[1], version: match[2], source: match[3] };
}

function parseLock(text, label) {
  const blocks = text.split(/(?:^|\n)\[\[package\]\]\n/).slice(1);
  if (!blocks.length) throw new Error(`${label} has no [[package]] entries`);
  return blocks.map((block, index) => {
    const context = `${label} [[package]] #${index + 1}`;
    const name = field(block, "name");
    const version = field(block, "version");
    if (!name || !version) throw new Error(`${context} is missing name or version`);
    const dependencyBlock = block.match(/^dependencies = \[\n([\s\S]*?)^\]$/m)?.[1];
    const dependencies = dependencyBlock
      ? dependencyBlock
          .split("\n")
          .map((line) => line.trim())
          .filter(Boolean)
          .map((line) => {
            const quoted = line.match(/^"([^"]+)",$/)?.[1];
            if (!quoted) throw new Error(`${context} has malformed dependency line ${JSON.stringify(line)}`);
            return quoted;
          })
      : [];
    return {
      name,
      version,
      source: field(block, "source"),
      checksum: field(block, "checksum"),
      dependencies,
    };
  });
}

function resolveReference(packages, value, context) {
  const reference = dependencyReference(value, context);
  const matches = packages.filter(
    (pkg) =>
      pkg.name === reference.name &&
      (!reference.version || pkg.version === reference.version) &&
      (!reference.source || pkg.source === reference.source),
  );
  if (matches.length !== 1) {
    const candidates = packages
      .filter((pkg) => pkg.name === reference.name)
      .map(packageIdentity)
      .sort()
      .join(", ");
    throw new Error(
      `${context} dependency ${JSON.stringify(value)} resolves to ${matches.length} packages` +
        (candidates ? `; candidates: ${candidates}` : "; no package with that name exists"),
    );
  }
  return matches[0];
}

function packageRoot(packages, label) {
  const matches = packages.filter((pkg) => pkg.name === SEARCH_PACKAGE);
  if (matches.length !== 1) {
    throw new Error(`${label} contains ${matches.length} ${SEARCH_PACKAGE} packages; expected exactly one`);
  }
  return matches[0];
}

function resolvedDependencies(packages, pkg, label) {
  return pkg.dependencies.map((dependency) =>
    resolveReference(packages, dependency, `${label} package ${packageIdentity(pkg)}`),
  );
}

function closure(packages, label) {
  const root = packageRoot(packages, label);
  const visited = new Set();
  const entries = [];
  const visit = (pkg) => {
    const identity = packageIdentity(pkg);
    if (visited.has(identity)) return;
    visited.add(identity);
    const dependencies = resolvedDependencies(packages, pkg, label);
    entries.push({
      name: pkg.name,
      version: pkg.version,
      source: pkg.source ?? null,
      checksum: pkg.checksum ?? null,
      dependencies: dependencies.map(packageIdentity).sort(),
    });
    for (const dependency of dependencies) visit(dependency);
  };
  visit(root);
  entries.sort((left, right) => {
    const leftIdentity = packageIdentity(left);
    const rightIdentity = packageIdentity(right);
    return leftIdentity < rightIdentity ? -1 : leftIdentity > rightIdentity ? 1 : 0;
  });
  return { root, entries };
}

function readLock(root, relative) {
  return parseLock(fs.readFileSync(path.join(root, relative), "utf8"), relative);
}

export function searchLockAlignmentProblems(root) {
  try {
    const wasmPackages = readLock(root, WASM_LOCK);
    const nativePackages = readLock(root, NATIVE_LOCK);
    const wasmRoot = packageRoot(wasmPackages, WASM_LOCK);
    const nativeRoot = packageRoot(nativePackages, NATIVE_LOCK);
    const problems = [];
    const visited = new Set();

    const compare = (wasmPackage, nativePackage) => {
      const pair = `${packageIdentity(wasmPackage)}\0${packageIdentity(nativePackage)}`;
      if (visited.has(pair)) return;
      visited.add(pair);

      for (const key of ["version", "source", "checksum"]) {
        const wasmValue = wasmPackage[key] ?? null;
        const nativeValue = nativePackage[key] ?? null;
        if (wasmValue !== nativeValue) {
          problems.push(
            `${packageIdentity(wasmPackage)} ${key} differs: Wasm ${wasmValue ?? "missing"}, native ${nativeValue ?? "missing"}`,
          );
        }
      }

      const wasmDependencies = resolvedDependencies(wasmPackages, wasmPackage, WASM_LOCK);
      const nativeDependencies = resolvedDependencies(nativePackages, nativePackage, NATIVE_LOCK);
      for (const wasmDependency of wasmDependencies) {
        const matches = nativeDependencies.filter((candidate) => candidate.name === wasmDependency.name);
        if (matches.length !== 1) {
          problems.push(
            `${packageIdentity(nativePackage)} has ${matches.length} resolved ${wasmDependency.name} dependencies; ` +
              `expected one corresponding to the Wasm search closure`,
          );
          continue;
        }
        compare(wasmDependency, matches[0]);
      }
    };

    compare(wasmRoot, nativeRoot);
    return problems;
  } catch (error) {
    return [error.message];
  }
}

function rustFilesBelow(root, relative) {
  const absolute = path.join(root, relative);
  const entries = fs.readdirSync(absolute, { withFileTypes: true });
  return entries.flatMap((entry) => {
    const child = path.posix.join(relative, entry.name);
    if (entry.isDirectory()) return rustFilesBelow(root, child);
    return entry.isFile() && entry.name.endsWith(".rs") ? [child] : [];
  });
}

function sourceFiles(root) {
  return [
    "crates/tine-search/Cargo.toml",
    ...rustFilesBelow(root, "crates/tine-search/src"),
    "crates/lsdoc-wasm/Cargo.toml",
    ...rustFilesBelow(root, "crates/lsdoc-wasm/src"),
  ].sort();
}

function sha256(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

export function searchSourceFingerprint(root) {
  const files = sourceFiles(root).map((relative) => ({
    path: relative,
    sha256: sha256(fs.readFileSync(path.join(root, relative))),
  }));
  const wasmClosure = closure(readLock(root, WASM_LOCK), WASM_LOCK).entries;
  const payload = JSON.stringify({ schema: 1, files, wasmClosure });
  return sha256(payload);
}

function vendoredStamp(root) {
  const bytes = fs.readFileSync(path.join(root, VENDORED_BYTES), "utf8");
  return bytes.match(/SEARCH_SOURCE_SHA256\s*=\s*"([0-9a-f]{64})"/)?.[1];
}

function wasmSizeCeiling(root) {
  const value = JSON.parse(fs.readFileSync(path.join(root, WASM_SIZE_CEILING), "utf8"))?.maxDecodedRawBytes;
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new Error(`${WASM_SIZE_CEILING} maxDecodedRawBytes must be a positive integer`);
  }
  return value;
}

function vendoredWasmRawBytes(root) {
  const source = fs.readFileSync(path.join(root, VENDORED_BYTES), "utf8");
  const encoded = source.match(/WASM_B64\s*=\s*"([A-Za-z0-9+/]*={0,2})"/)?.[1];
  if (!encoded) throw new Error(`${VENDORED_BYTES} has no decodable WASM_B64 payload`);
  const decoded = Buffer.from(encoded, "base64");
  if (decoded.toString("base64") !== encoded) {
    throw new Error(`${VENDORED_BYTES} WASM_B64 payload is not canonical base64`);
  }
  return decoded.length;
}

export function vendoredWasmSizeProblems(root) {
  try {
    const ceiling = wasmSizeCeiling(root);
    const actual = vendoredWasmRawBytes(root);
    return actual > ceiling
      ? [`vendored Wasm decoded raw size is ${actual} bytes; ceiling is ${ceiling} bytes from ${WASM_SIZE_CEILING}`]
      : [];
  } catch (error) {
    return [error.message];
  }
}

export function wasmSearchGuardProblems(root) {
  const problems = [...searchLockAlignmentProblems(root), ...vendoredWasmSizeProblems(root)];
  let expected;
  try {
    expected = searchSourceFingerprint(root);
  } catch (error) {
    problems.push(`could not compute shared-search fingerprint: ${error.message}`);
    return problems;
  }
  let stamped;
  try {
    stamped = vendoredStamp(root);
  } catch (error) {
    problems.push(`could not read ${VENDORED_BYTES}: ${error.message}`);
    return problems;
  }
  if (!stamped) {
    problems.push(`${VENDORED_BYTES} has no SEARCH_SOURCE_SHA256 stamp`);
  } else if (stamped !== expected) {
    problems.push(`vendored Wasm shared-search stamp is ${stamped}; expected ${expected}`);
  }
  return problems;
}
