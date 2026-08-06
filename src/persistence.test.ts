import { describe, expect, it } from "vitest";
import { flushAll, isRetryableSaveFailure, trackAssetWrite } from "./persistence";

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (reason?: unknown) => void } {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe("asset write close barrier", () => {
  it("flushAll waits for a pending tracked asset write", async () => {
    const asset = deferred<string>();
    const tracked = trackAssetWrite(asset.promise);
    let flushed = false;

    const flush = flushAll().then((ok) => {
      flushed = true;
      return ok;
    });
    await Promise.resolve();
    expect(flushed).toBe(false);

    asset.resolve("saved.png");

    await expect(tracked).resolves.toBe("saved.png");
    await expect(flush).resolves.toBe(true);
    expect(flushed).toBe(true);
  });
});

describe("save failure classification", () => {
  // A non-retryable failure used to be retried twice more before toasting, and
  // each retry re-runs the whole pre-save check — on a large graph, the
  // expensive part — only to reach the same answer. GH #267's "about a minute
  // then a red toast" is that multiplier on top of an already slow check.
  it("does not retry failures a retry cannot change", () => {
    for (const code of [
      "precheck.symlink",
      "precheck.portable_collision",
      "precheck.resource_alias",
      "precheck.not_portable",
      "precheck.nofollow",
      "precheck.limit",
      "identity.owned_elsewhere",
      // A name collision is real, but no number of retries frees the name.
      "identity.name_taken",
    ]) {
      expect(isRetryableSaveFailure(`${code}: something specific`)).toBe(false);
    }
  });

  it("still retries failures that a later attempt can succeed at", () => {
    // The graph moved under the capture (a sync client mid-pull), or the file
    // was replaced and the watcher has not re-pinned its identity yet. Both
    // resolve on their own.
    expect(isRetryableSaveFailure("precheck.interrupted: inventory changed")).toBe(true);
    expect(isRetryableSaveFailure("identity.changed_since_load: ...")).toBe(true);
    expect(isRetryableSaveFailure("unknown: disk full")).toBe(true);
    expect(isRetryableSaveFailure(new Error("EBUSY"))).toBe(true);
  });
});
