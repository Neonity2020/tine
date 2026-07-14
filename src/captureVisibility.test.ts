import { describe, expect, it, vi } from "vitest";
import { createCaptureBlurGate, resettleIfVisible } from "./captureVisibility";

describe("resettleIfVisible", () => {
  it("recovers focus setup when the initial capture-shown event was missed", async () => {
    const resettle = vi.fn();

    await resettleIfVisible({ isVisible: async () => true }, resettle);

    expect(resettle).toHaveBeenCalledOnce();
  });

  it("does not focus a capture window that is still hidden", async () => {
    const resettle = vi.fn();

    await resettleIfVisible({ isVisible: async () => false }, resettle);

    expect(resettle).not.toHaveBeenCalled();
  });
});

describe("createCaptureBlurGate", () => {
  it("ignores the unfocused transition emitted while a first-show activation is pending", () => {
    const gate = createCaptureBlurGate();

    expect(gate.focusChanged(false)).toBe(false);
    expect(gate.focusChanged(false)).toBe(false);
  });

  it("dismisses once after the capture window has genuinely held focus", () => {
    const gate = createCaptureBlurGate();

    expect(gate.focusChanged(true)).toBe(false);
    expect(gate.focusChanged(false)).toBe(true);
    expect(gate.focusChanged(false)).toBe(false);
  });

  it("disarms a stale blur after an explicit hide", () => {
    const gate = createCaptureBlurGate();

    gate.focusChanged(true);
    gate.disarm();
    expect(gate.focusChanged(false)).toBe(false);
  });
});
