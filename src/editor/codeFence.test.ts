import { describe, expect, it } from "vitest";
import { codeFenceOnly } from "./codeFence";

describe("codeFenceOnly", () => {
  it("detects a whole-block markdown fence with a language", () => {
    expect(codeFenceOnly("```js\nconst x = 1;\nconsole.log(x);\n```", "md")).toEqual({ lang: "js" });
  });

  it("detects a fence with no language and tolerates missing closing fence (still typing)", () => {
    expect(codeFenceOnly("```\nraw code", "md")).toEqual({ lang: "" });
    expect(codeFenceOnly("```python\nx = 1", "md")).toEqual({ lang: "python" });
  });

  it("detects a tilde fence and an empty code body", () => {
    expect(codeFenceOnly("~~~sh\necho hi\n~~~", "md")).toEqual({ lang: "sh" });
    expect(codeFenceOnly("```\n```", "md")).toEqual({ lang: "" });
  });

  it("excludes calc fences (they have their own editor mode)", () => {
    expect(codeFenceOnly("```calc\n1 + 1\n```", "md")).toBeNull();
    expect(codeFenceOnly("```CALC\n1 + 1\n```", "md")).toBeNull();
  });

  it("rejects mixed content: text around the fence", () => {
    expect(codeFenceOnly("intro\n```js\nx\n```", "md")).toBeNull();
    expect(codeFenceOnly("```js\nx\n```\ntailing note", "md")).toBeNull();
    // ...but trailing BLANK lines are tolerated while composing.
    expect(codeFenceOnly("```js\nx\n```\n", "md")).toEqual({ lang: "js" });
  });

  it("rejects a second construct after a closed fence and fence-like inline code", () => {
    expect(codeFenceOnly("```js\nx\n```\n```py\ny", "md")).toBeNull();
    // An inner code line wrapped in single backticks is not a closer.
    expect(codeFenceOnly("```\n`weird`\nmore", "md")).toEqual({ lang: "" });
    expect(codeFenceOnly("`plain inline` no fence", "md")).toBeNull();
  });

  it("detects org #+BEGIN_SRC blocks", () => {
    expect(codeFenceOnly("#+BEGIN_SRC python\nx = 1\n#+END_SRC", "org")).toEqual({ lang: "python" });
    expect(codeFenceOnly("#+begin_src elisp\n(x 1)\n#+end_src", "org")).toEqual({ lang: "elisp" });
    expect(codeFenceOnly("#+BEGIN_SRC rust\nlet x = 1;", "org")).toEqual({ lang: "rust" });
  });

  it("detects org #+BEGIN_EXAMPLE blocks", () => {
    expect(codeFenceOnly("#+BEGIN_EXAMPLE\nstuff\n#+END_EXAMPLE", "org")).toEqual({ lang: "" });
    // An unclosed example block is still code-shaped (typing tolerance), same
    // as an unclosed markdown fence.
    expect(codeFenceOnly("#+BEGIN_EXAMPLE\nstuff\ntailing", "org")).toEqual({ lang: "" });
  });

  it("org src with text after the closer is not code-shaped", () => {
    expect(codeFenceOnly("#+BEGIN_SRC python\nx\n#+END_SRC\n/italic note/", "org")).toBeNull();
  });
});
