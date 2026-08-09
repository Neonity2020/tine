import { beforeEach, describe, expect, it } from "vitest";
import {
  collapsedGroupsFor,
  resetReferenceSectionState,
  sectionOverride,
  setCollapsedGroupsFor,
  setSectionOverride,
} from "./referenceSectionState";

beforeEach(() => {
  resetReferenceSectionState();
});

// GH #272: the section's expand/collapse choice must survive a remount of the
// component that renders it. That is only possible if it does not live there.
describe("reference section state", () => {
  it("has no opinion until the user expresses one", () => {
    // The distinction matters: "unset" lets the caller's OG default (collapse
    // above 100 references) apply, while `false` is an explicit user choice.
    expect(sectionOverride("linked", "Page")).toBeUndefined();
    setSectionOverride("linked", "Page", false);
    expect(sectionOverride("linked", "Page")).toBe(false);
  });

  it("keeps the two sections and the two pages independent", () => {
    setSectionOverride("linked", "Page A", false);
    setSectionOverride("unlinked", "Page A", true);
    setSectionOverride("linked", "Page B", true);
    expect(sectionOverride("linked", "Page A")).toBe(false);
    expect(sectionOverride("unlinked", "Page A")).toBe(true);
    expect(sectionOverride("linked", "Page B")).toBe(true);
    expect(sectionOverride("unlinked", "Page B")).toBeUndefined();
  });

  it("round-trips per-group collapse sets", () => {
    setCollapsedGroupsFor("linked", "Page", new Set(["one", "two"]));
    expect([...collapsedGroupsFor("linked", "Page")].sort()).toEqual(["one", "two"]);
    expect(collapsedGroupsFor("linked", "Other").size).toBe(0);
  });

  it("is cleared on a graph switch", () => {
    setSectionOverride("linked", "Page", false);
    setCollapsedGroupsFor("linked", "Page", new Set(["one"]));
    resetReferenceSectionState();
    expect(sectionOverride("linked", "Page")).toBeUndefined();
    expect(collapsedGroupsFor("linked", "Page").size).toBe(0);
  });
});
