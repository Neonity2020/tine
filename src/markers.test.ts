import { describe, it, expect } from "vitest";
import { MARKERS, OPEN_MARKERS, DONE_MARKERS, MARKER_RE } from "./markers";
import { leadingMarker } from "./editor/marker";

describe("task markers (single source of truth)", () => {
  it("matches the backend set (crates/tine-core/src/doc.rs MARKERS) — keep in sync", () => {
    // If doc.rs::MARKERS changes, update this list (and vice-versa). The two can't
    // share a literal across the language boundary, so this is the drift guard.
    expect([...MARKERS].sort()).toEqual(
      [
        "CANCELED", "CANCELLED", "DOING", "DONE", "IN-PROGRESS",
        "LATER", "NOW", "TODO", "WAIT", "WAITING",
      ].sort()
    );
  });

  it("OPEN ∪ DONE partitions MARKERS with no overlap", () => {
    expect(OPEN_MARKERS.size + DONE_MARKERS.size).toBe(MARKERS.length);
    for (const m of MARKERS) {
      expect(OPEN_MARKERS.has(m) !== DONE_MARKERS.has(m)).toBe(true); // exactly one
    }
    // The drift that prompted this: IN-PROGRESS and WAIT are OPEN (carried forward).
    expect(OPEN_MARKERS.has("IN-PROGRESS")).toBe(true);
    expect(OPEN_MARKERS.has("WAIT")).toBe(true);
    expect(DONE_MARKERS.has("CANCELLED")).toBe(true);
  });

  it("MARKER_RE anchors every marker as a whole word, prefix-safe (WAITING vs WAIT)", () => {
    for (const m of MARKERS) {
      expect(MARKER_RE.exec(`${m} do the thing`)?.[1]).toBe(m);
      expect(leadingMarker(`${m} do the thing`)).toBe(m);
    }
    // "WAITING" must not be read as "WAIT".
    expect(MARKER_RE.exec("WAITING x")?.[1]).toBe("WAITING");
    // A non-marker word isn't matched.
    expect(MARKER_RE.exec("TODOLIST x")).toBeNull();
  });
});
