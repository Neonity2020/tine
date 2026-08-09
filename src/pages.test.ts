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
// ROW. A correctness test cannot see that, so pin the shape directly: building
// the index once and asking it N times must stay linear in the list, not
// quadratic. Measured before the change: 21 ms at 5,000 pages and 39 ms at
// 20,000, for the sidebar's 300 visible rows.
describe("pageListLabels scales with the list, not with list x rows", () => {
  const list = (n: number) =>
    Array.from({ length: n }, (_, i) => page(`p${i}`, `pages/p${i}.md`));

  function costOfLabellingEveryRow(n: number): number {
    const pages = list(n);
    const started = performance.now();
    const label = pageListLabels(pages);
    for (const p of pages) label(p);
    return performance.now() - started;
  }

  it("stays roughly linear when the list grows tenfold", () => {
    costOfLabellingEveryRow(2_000); // warm the JIT so the first run isn't the slow one
    const small = costOfLabellingEveryRow(1_000);
    const large = costOfLabellingEveryRow(10_000);

    // Quadratic would be ~100x. Allow a very loose ceiling: this is a shape
    // assertion on a shared CI box, not a benchmark.
    expect(large).toBeLessThan(Math.max(small, 0.5) * 25);
  });

  it("still disambiguates correctly at scale", () => {
    const pages = [...list(5_000), page("p0", "pages/other/p0.md")];
    const label = pageListLabels(pages);
    expect(label(pages[0])).toBe("p0 — pages/");
    expect(label(pages[pages.length - 1])).toBe("p0 — other/");
    expect(label(pages[1])).toBe("p1");
  });
});
