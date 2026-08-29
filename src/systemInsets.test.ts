import { describe, expect, it } from "vitest";
import { installSystemInsetOwner, systemInsetOwner } from "./systemInsets";

describe("system inset ownership", () => {
  it.each([
    [true, "Mozilla/5.0 (Linux; Android 15)", "native-viewport"],
    [false, "Mozilla/5.0 (Linux; Android 15)", "css-viewport"],
    [true, "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0)", "css-viewport"],
    [true, "Mozilla/5.0 (X11; Linux x86_64)", "css-viewport"],
  ] as const)("selects the sole owner for native=%s userAgent=%s", (nativeHost, userAgent, expected) => {
    expect(systemInsetOwner(nativeHost, userAgent)).toBe(expected);
  });

  it("publishes the owner as the CSS contract", () => {
    const root = { dataset: {} } as HTMLElement;
    expect(installSystemInsetOwner(root, true, "Android")).toBe("native-viewport");
    expect(root.dataset.systemInsets).toBe("native-viewport");
    expect(installSystemInsetOwner(root, false, "Android")).toBe("css-viewport");
    expect(root.dataset.systemInsets).toBe("css-viewport");
  });
});
