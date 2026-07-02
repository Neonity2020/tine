import { describe, expect, it } from "vitest";
import { commandDefaults, eventToBindingString } from "./keybindings";

function keyEvent(init: Partial<KeyboardEvent>): KeyboardEvent {
  return {
    key: "",
    code: "",
    shiftKey: false,
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    ...init,
  } as KeyboardEvent;
}

describe("keyboard binding strings", () => {
  it("serializes Shift+/ as shift+? because KeyboardEvent.key is already shifted", () => {
    expect(eventToBindingString(keyEvent({ key: "?", code: "Slash", shiftKey: true }))).toBe("shift+?");
  });

  it("normalizes synthetic Shift+/ events that report key slash", () => {
    expect(eventToBindingString(keyEvent({ key: "/", code: "Slash", shiftKey: true }))).toBe("shift+?");
  });

  it("binds help and keyboard-shortcuts commands to non-editing shortcuts", () => {
    const byId = Object.fromEntries(commandDefaults().map((c) => [c.id, c]));

    expect(byId["ui/toggle-help"]).toMatchObject({ binding: "shift+?", scope: "select" });
    expect(byId["go/keyboard-shortcuts"]).toMatchObject({ binding: "g s", scope: "select" });
  });
});
