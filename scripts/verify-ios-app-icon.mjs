import assert from "node:assert/strict";
import process from "node:process";
import { createCanvas, loadImage } from "@napi-rs/canvas";

const [, , expectedPath, bundledPath] = process.argv;
if (!expectedPath || !bundledPath) {
  throw new Error(
    "usage: verify-ios-app-icon.mjs <tracked-icon.png> <decoded-bundled-icon.png>",
  );
}

async function rgba(path) {
  const image = await loadImage(path);
  const canvas = createCanvas(image.width, image.height);
  const context = canvas.getContext("2d");
  context.drawImage(image, 0, 0);
  return {
    width: image.width,
    height: image.height,
    pixels: Buffer.from(
      context.getImageData(0, 0, image.width, image.height).data,
    ),
  };
}

const [expected, bundled] = await Promise.all([
  rgba(expectedPath),
  rgba(bundledPath),
]);
assert.deepEqual(
  { width: bundled.width, height: bundled.height },
  { width: expected.width, height: expected.height },
  "the signed IPA primary icon dimensions differ from Tine's tracked icon",
);
assert.equal(
  Buffer.compare(bundled.pixels, expected.pixels),
  0,
  "the signed IPA primary icon pixels differ from Tine's tracked icon",
);
console.log(
  `verified ${bundled.width}x${bundled.height} signed IPA icon pixels`,
);
