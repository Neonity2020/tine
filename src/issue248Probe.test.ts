import { expect, it } from "vitest";
import { measureIssue248Async } from "./issue248Probe";

it("returns the original async operation when performance collection is disabled", () => {
  const operation = Promise.resolve("saved");

  expect(measureIssue248Async("frontend.savePageAwaitMs", () => operation)).toBe(operation);
});
