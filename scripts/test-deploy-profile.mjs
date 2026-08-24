#!/usr/bin/env node

import assert from "node:assert/strict";
import fs from "node:fs";

const deploy = fs.readFileSync(new URL("./deploy.sh", import.meta.url), "utf8");
const cargo = fs.readFileSync(new URL("../Cargo.toml", import.meta.url), "utf8");

assert.match(cargo, /\[profile\.release\]\s+codegen-units = 1\b/);

const fastBlock = deploy.match(
  /if \[ "\$\{TINE_FAST_LOCAL_BUILD:-0\}" = "1" \]; then([\s\S]*?)\nfi/,
);
assert.ok(fastBlock, "deploy.sh is missing the explicit fast-local opt-in block");
assert.match(fastBlock[1], /CARGO_PROFILE_RELEASE_CODEGEN_UNITS=.*:-16/);
assert.match(fastBlock[1], /CARGO_PROFILE_RELEASE_INCREMENTAL=.*:-true/);

const outsideFastBlock = deploy.replace(fastBlock[0], "");
assert.doesNotMatch(
  outsideFastBlock,
  /CARGO_PROFILE_RELEASE_(?:CODEGEN_UNITS|INCREMENTAL)=/,
  "deterministic release defaults must not be overridden outside the opt-in block",
);
assert.match(
  deploy,
  /cargo build --release --features custom-protocol --manifest-path src-tauri\/Cargo\.toml/,
  "fast-local deployment must retain the production protocol and release output path",
);

console.log("deploy profile contract OK: deterministic default, explicit fast-local opt-in");
