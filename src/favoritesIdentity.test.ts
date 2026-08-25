// DUP-2 (2026-08-25 duplication audit): favorites membership used four different
// predicates — exact-match `isFavorite`, case-folded sidebar layout, kind-blind
// delete, exact rename dedupe — so the star button, the sidebar, and config.edn
// could each give a different answer to "is this page a favorite?". These tests
// pin the one shared identity: kind-scoped, alias-resolved, pageIdentityKey-folded.
// They deliberately use only exports that predate the fix so the necessity gate
// can run them against pre-fix code.
import { beforeEach, describe, expect, it } from "vitest";
import {
  favorites,
  isFavorite,
  removeDeletedPageFromNavigation,
  renamePageInNavigation,
  setAliasMap,
  setFavorites,
  toggleFavorite,
} from "./ui";
import { reconcileLayout } from "./favoritesLayout";

beforeEach(() => {
  setAliasMap({});
  setFavorites([]);
});

describe("favorite membership identity (DUP-2)", () => {
  it("a page starred under one spelling reads as favorited under another case", () => {
    setFavorites([{ name: "Foo", kind: "page" }]);
    expect(isFavorite("foo")).toBe(true);
  });

  it("NFC and NFD spellings are one favorite", () => {
    setFavorites([{ name: "Caf\u00e9", kind: "page" }]); // NFC
    expect(isFavorite("Cafe\u0301")).toBe(true); // NFD
  });

  it("toggling under another spelling unstars instead of appending a duplicate", () => {
    setFavorites([{ name: "Foo", kind: "page" }]);
    toggleFavorite("foo", "page");
    expect(favorites()).toEqual([]);
  });

  it("deleting a page does not drop a journal favorite that shares the name", () => {
    setFavorites([{ name: "Aug 25th, 2026", kind: "journal" }]);
    removeDeletedPageFromNavigation("Aug 25th, 2026", "page");
    expect(favorites()).toEqual([{ name: "Aug 25th, 2026", kind: "journal" }]);
  });

  it("rename re-keys a favorite stored under a different spelling of the page", () => {
    setFavorites([{ name: "foo", kind: "page" }]);
    renamePageInNavigation("Foo", "Bar");
    expect(favorites()).toEqual([{ name: "Bar", kind: "page" }]);
  });

  it("sidebar arrangement agrees with membership on NFC/NFD twins", () => {
    // Pre-fix the layout folded with bare trim+toLowerCase, so the two
    // spellings below produced TWO sidebar rows for one page.
    const layout = reconcileLayout([], ["Caf\u00e9", "Cafe\u0301"]); // NFC + NFD
    expect(layout).toHaveLength(1);
  });
});
