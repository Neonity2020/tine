// Guard: the extra files the Linux packages install (the CLI man pages) must be
// accepted by the package builders, and deb and rpm must ship the same set.
//
// Tauri hands `bundle.linux.rpm.files` destinations to the rpm crate verbatim,
// and the rpm crate rejects any destination that does not start with `/` or
// `./` ("invalid destination path ... invalid start, expected / or ./"). The
// deb bundler accepts a relative `usr/...` key, so a copy of the deb block
// looks fine locally and fails only in the hosted release build: v0.6.985's
// first candidate lost both Linux bundles that way. Nothing else in the local
// gate set runs the rpm bundler, so this test is the local signal.

import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const confPath = fileURLToPath(new URL("../src-tauri/tauri.conf.json", import.meta.url));
const conf = JSON.parse(readFileSync(confPath, "utf8")) as {
  bundle: { linux: { deb: { files: Record<string, string> }; rpm: { files: Record<string, string> } } };
};
const deb = conf.bundle.linux.deb.files;
const rpm = conf.bundle.linux.rpm.files;
const normalize = (dest: string) => dest.replace(/^\.?\//, "");

describe("Linux package extra files", () => {
  it("gives every rpm destination the absolute form the rpm crate requires", () => {
    const bad = Object.keys(rpm).filter((dest) => !dest.startsWith("/") && !dest.startsWith("./"));
    expect(bad).toEqual([]);
  });

  it("installs the same files from the same sources in deb and rpm", () => {
    const pairs = (files: Record<string, string>) =>
      Object.entries(files)
        .map(([dest, src]) => `${normalize(dest)} <- ${src}`)
        .sort();
    expect(pairs(rpm)).toEqual(pairs(deb));
  });

  it("points every packaged file at a checked-in source", () => {
    const missing = Object.values({ ...deb, ...rpm }).filter(
      (src) => !existsSync(resolve(dirname(confPath), src)),
    );
    expect(missing).toEqual([]);
  });
});
