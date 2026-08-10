import { describe, expect, it } from "vitest";
import { pageListLabel, pageListLabels } from "./pages";
import type { PageEntry } from "./types";

const page = (name: string, path: string): PageEntry => ({
  name,
  kind: "page",
  date_key: null,
  path,
});

describe("pageListLabel", () => {
  it("leaves unique page names unchanged", () => {
    const pages = [page("foo", "pages/client-a/foo.md"), page("bar", "pages/client-b/bar.md")];
    expect(pageListLabel(pages[0], pages)).toBe("foo");
  });

  it("adds the parent sub-path for colliding display names", () => {
    const pages = [page("foo", "pages/client-a/foo.md"), page("foo", "pages/client-b/foo.md")];
    expect(pageListLabel(pages[0], pages)).toBe("foo — client-a/");
    expect(pageListLabel(pages[1], pages)).toBe("foo — client-b/");
  });

  it("falls back to the full path when the parent sub-path is still ambiguous", () => {
    const pages = [page("foo", "pages/client-a/foo.md"), page("foo", "pages/client-a/foo.org")];
    expect(pageListLabel(pages[0], pages)).toBe("foo — pages/client-a/foo.md");
    expect(pageListLabel(pages[1], pages)).toBe("foo — pages/client-a/foo.org");
  });
});

// Direct Files performance audit, finding F9. The label rule is unchanged; what
// changed is that deciding it no longer costs a scan of the whole page list PER
// ROW. A correctness test cannot see that, and a wall-clock test on a shared box
// is a coin flip — so count the work directly instead: a linear implementation
// reads the array once while BUILDING the index and never touches it again,
// while a per-row scan reads it once more for every row it labels.
describe("pageListLabels reads the page list once, not once per row", () => {
  function countingList(n: number) {
    const pages = Array.from({ length: n }, (_, i) => page(`p${i}`, `pages/p${i}.md`));
    let reads = 0;
    const watched = new Proxy(pages, {
      get(target, key, receiver) {
        if (typeof key === "string" && /^\d+$/.test(key)) reads++;
        return Reflect.get(target, key, receiver);
      },
    });
    return { pages, watched, reads: () => reads };
  }

  it("touches no list element while labelling", () => {
    const { pages, watched, reads } = countingList(500);

    const label = pageListLabels(watched);
    const afterBuild = reads();
    for (const p of pages) label(p);

    expect(afterBuild).toBeLessThanOrEqual(pages.length);
    expect(reads() - afterBuild).toBe(0);
  });

  // Same assertion stated as the failure it catches: the pre-fix implementation
  // read ~2 x 500 x 500 elements to label this list.
  it("does not grow its element reads with the number of rows labelled", () => {
    const { pages, watched, reads } = countingList(500);
    const label = pageListLabels(watched);

    for (const p of pages.slice(0, 10)) label(p);
    const afterTen = reads();
    for (const p of pages) label(p);

    expect(reads()).toBe(afterTen);
  });

  it("still disambiguates correctly at scale", () => {
    const pages = [
      ...Array.from({ length: 5_000 }, (_, i) => page(`p${i}`, `pages/p${i}.md`)),
      page("p0", "pages/other/p0.md"),
    ];
    const label = pageListLabels(pages);
    expect(label(pages[0])).toBe("p0 — pages/");
    expect(label(pages[pages.length - 1])).toBe("p0 — other/");
    expect(label(pages[1])).toBe("p1");
  });
});
