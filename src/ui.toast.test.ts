import { afterEach, describe, expect, it } from "vitest";
import { pushToast, setToasts, toasts } from "./ui";

describe("toast deduplication", () => {
  afterEach(() => setToasts([]));

  it("keeps one visible report for repeated identical runtime diagnostics", () => {
    const message = "Tine-managed storage needs attention: repeated failure";
    const first = pushToast(message, "error", { sticky: true, dedupe: true });
    const repeated = pushToast(message, "error", { sticky: true, dedupe: true });

    expect(repeated).toBe(first);
    expect(toasts()).toHaveLength(1);

    pushToast(`${message}: another condition`, "error", { sticky: true, dedupe: true });
    expect(toasts()).toHaveLength(2);
  });
});
