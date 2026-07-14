import { describe, expect, it } from "vitest";
import { SELECTION_ACTIONS, essentialSelectionActions, secondarySelectionActions } from "./selectionActions";

describe("selection action registry", () => {
  it("exposes stable unique identities and keeps page links and code essential", () => {
    const ids = SELECTION_ACTIONS.map((action) => action.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(essentialSelectionActions.map((action) => action.id)).toEqual([
      "bold", "italic", "page-link", "inline-code",
    ]);
    expect(secondarySelectionActions.map((action) => action.id)).toEqual([
      "link", "strikethrough", "highlight",
    ]);
  });

  it("wraps and unwraps page links and inline code without losing the inner selection", () => {
    const page = SELECTION_ACTIONS.find((action) => action.id === "page-link")!;
    const code = SELECTION_ACTIONS.find((action) => action.id === "inline-code")!;
    const wrappedPage = page.apply("alpha beta", 0, 5);
    expect(wrappedPage).toEqual({ text: "[[alpha]] beta", start: 2, end: 7 });
    expect(page.apply(wrappedPage.text, wrappedPage.start, wrappedPage.end)).toEqual({
      text: "alpha beta", start: 0, end: 5,
    });
    expect(code.apply("alpha beta", 0, 5)).toEqual({
      text: "`alpha` beta", start: 1, end: 6,
    });
  });
});
