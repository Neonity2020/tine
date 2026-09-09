// **The Pages half of a mixed result** (SPEC §7.6, Q3).
//
// A page is not a block. A block result renders through the ordinary editable
// block renderers and keeps its editing behaviour; a page result is a
// NAVIGATION row whose columns come from the page's OWN authored properties.
// Before this, both went through one presentation switch keyed on `hit.entity`,
// so a page could only ever be a link with the block table's headers over it.
//
// The two rules this file pins:
//
//  * **It renders what the backend returned, in the order it returned it.**
//    Ordering and sampling happen in SQL over the COMPLETE matched set (Q4), so
//    a frontend re-sort would silently replace a complete answer with an answer
//    about whichever rows happened to fit.
//  * **Stored pages key by physical path and kind.** Two pages can share a
//    display name at two paths, and a key that used only the name would make one
//    of them disappear and the other take its clicks.

import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { createSignal, type JSX } from "solid-js";
import { QueryPageResults, pageHitKey, type QueryPageHit } from "./QueryPageResults";
import type { ViewSettings } from "../editor/queryIr";

afterEach(() => {
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

/** One stored page hit, with the page's own ordered properties. */
function pageHit(
  name: string,
  path: string,
  properties: [string, string][] = [],
  extra: Partial<QueryPageHit> = {},
): QueryPageHit {
  return {
    entity: "page",
    page: { name, kind: "page", date_key: null, path },
    display_text: name,
    evidence: [],
    score: 10,
    row: { name, kind: "page", path, properties },
    ...extra,
  };
}

describe("QueryPageResults", () => {
  it("q3_page_rows_key_by_path_and_kind_not_display_name", () => {
    const twinA = pageHit("Research", "pages/client-a/Research.md");
    const twinB = pageHit("Research", "pages/client-b/Research.md");
    expect(pageHitKey(twinA)).not.toBe(pageHitKey(twinB));

    // A virtual reference-name suggestion names no stored page, so it keys by
    // the name it IS — that is its whole identity.
    const virtual: QueryPageHit = {
      entity: "page",
      page: { name: "Unwritten", kind: "page", date_key: null, path: "" },
      display_text: "Unwritten",
      evidence: [],
      score: 1,
    };
    expect(pageHitKey(virtual)).toBe("name\0Unwritten");

    const { root, dispose } = mount(() => (
      <QueryPageResults hits={() => [twinA, twinB, virtual]} view={() => ({})} onOpen={() => {}} />
    ));
    try {
      const keys = [...root.querySelectorAll("[data-page-key]")].map((el) => el.getAttribute("data-page-key"));
      expect(new Set(keys).size).toBe(3);
      expect(root.querySelectorAll(".query-page-link")).toHaveLength(3);
    } finally {
      dispose();
    }
  });

  it("q3_page_presentations_render_their_own_dom_contract", () => {
    const hits = [
      pageHit("Alpha", "pages/Alpha.md", [["status", "open"]]),
      pageHit("Beta", "pages/Beta.md", [["status", "done"]]),
    ];
    const [view, setView] = createSignal<ViewSettings>({ view: "list" });
    const { root, dispose } = mount(() => (
      <QueryPageResults hits={() => hits} view={view} onOpen={() => {}} />
    ));
    try {
      expect(root.querySelector<HTMLElement>(".query-results-list")?.getAttribute("aria-label"))
        .toBe("Page results");
      expect(root.querySelectorAll(".query-results-list > li")).toHaveLength(2);

      setView({ view: "search" });
      const search = root.querySelector<HTMLElement>('[role="list"]')!;
      expect(search.getAttribute("aria-label")).toBe("Page results");
      expect(search.querySelectorAll('[role="listitem"]')).toHaveLength(2);
      // The listitem CONTAINS the navigation control rather than being it: a
      // button that is also the row has no row semantics left to announce.
      expect(search.querySelector('[role="listitem"] .query-page-link')).not.toBeNull();

      // A native table with a matching caption and real column headers. The
      // columns are the page's OWN authored properties, not a block schema.
      setView({ view: "table", columns: ["name", "status"] });
      const table = root.querySelector("table.query-results-table")!;
      expect(table.querySelector("caption")?.textContent).toBe("Page results");
      expect([...table.querySelectorAll("th")].map((th) => th.getAttribute("scope"))).toEqual(["col", "col", "col"]);
      expect([...table.querySelectorAll("thead th")].map((th) => th.textContent))
        .toEqual(["Page", "Name", "status"]);
      expect([...table.querySelectorAll("tbody tr")]).toHaveLength(2);
      expect(table.querySelectorAll("tbody tr")[0].textContent).toContain("open");

      setView({ view: "board", group_by: "prop:status" });
      const columns = [...root.querySelectorAll<HTMLElement>(".query-board-column")];
      expect(columns.map((column) => column.getAttribute("aria-label"))).toEqual(["open", "done"]);
      expect(columns[0].querySelector('[role="list"]')?.getAttribute("aria-label")).toBe("Page results");
      expect(columns[0].querySelectorAll('[role="listitem"] .query-page-link')).toHaveLength(1);
    } finally {
      dispose();
    }
  });

  it("q3_page_board_groups_by_adjacency_and_never_reorders_the_backends_answer", () => {
    // The backend sorted these; "open" legitimately opens twice. Re-clustering
    // by value would move `Gamma` above `Beta` — an answer the query never gave.
    const hits = [
      pageHit("Alpha", "pages/Alpha.md", [["status", "open"]]),
      pageHit("Beta", "pages/Beta.md", [["status", "done"]]),
      pageHit("Gamma", "pages/Gamma.md", [["status", "open"]]),
    ];
    const { root, dispose } = mount(() => (
      <QueryPageResults hits={() => hits} view={() => ({ view: "board", group_by: "prop:status" })} onOpen={() => {}} />
    ));
    try {
      const columns = [...root.querySelectorAll<HTMLElement>(".query-board-column")];
      expect(columns.map((column) => column.getAttribute("aria-label"))).toEqual(["open", "done", "open"]);
      expect([...root.querySelectorAll(".query-page-name")].map((el) => el.textContent))
        .toEqual(["Alpha", "Beta", "Gamma"]);
    } finally {
      dispose();
    }
  });

  it("q3_page_rows_never_resort_or_truncate_what_the_backend_returned", () => {
    // A sort and a sample were already applied over the COMPLETE matched set in
    // SQL. Applying either again here would be a second, weaker answer.
    const hits = ["Zulu", "Alpha", "Mike"].map((name) => pageHit(name, `pages/${name}.md`));
    const { root, dispose } = mount(() => (
      <QueryPageResults
        hits={() => hits}
        view={() => ({ view: "list", sort: [["name", "asc"]], sample: 1 })}
        onOpen={() => {}}
      />
    ));
    try {
      expect([...root.querySelectorAll(".query-page-name")].map((el) => el.textContent))
        .toEqual(["Zulu", "Alpha", "Mike"]);
    } finally {
      dispose();
    }
  });

  it("q3_page_row_navigation_carries_the_hosts_link_gestures", () => {
    const opened: [string, boolean][] = [];
    const auxed: string[] = [];
    const hit = pageHit("Alpha", "pages/Alpha.md");
    const { root, dispose } = mount(() => (
      <QueryPageResults
        hits={() => [hit]}
        view={() => ({ view: "list" })}
        onOpen={(opening, event) => opened.push([opening.page.name, event.ctrlKey])}
        linkClass="query-search-page"
        linkAttrs={(target) => ({ onAuxClick: () => auxed.push(target.page.name) })}
      />
    ));
    try {
      const link = root.querySelector<HTMLButtonElement>(".query-page-link.query-search-page")!;
      expect(link.dataset.pagePath).toBe("pages/Alpha.md");
      expect(link.dataset.pageKind).toBe("page");
      link.dispatchEvent(new MouseEvent("click", { bubbles: true, ctrlKey: true }));
      expect(opened).toEqual([["Alpha", true]]);
      link.dispatchEvent(new MouseEvent("auxclick", { bubbles: true }));
      expect(auxed).toEqual(["Alpha"]);
    } finally {
      dispose();
    }
  });

  it("q3_page_excerpt_is_shown_only_when_it_is_not_the_page_name", () => {
    const named = pageHit("Alpha", "pages/Alpha.md", [], {
      evidence: [{ clause_id: 1, field: "page_name", mode: "contains", spans: [{ start: 0, end: 5 }], score: 1 }],
    });
    const byContent = pageHit("Beta", "pages/Beta.md", [], {
      display_text: "a body line that mentions alpha",
      evidence: [{ clause_id: 1, field: "visible_content", mode: "contains", spans: [{ start: 26, end: 31 }], score: 1 }],
    });
    const { root, dispose } = mount(() => (
      <QueryPageResults hits={() => [named, byContent]} view={() => ({ view: "list" })} onOpen={() => {}} />
    ));
    try {
      // The name-matched page marks its NAME — `page_name` evidence indexes the
      // name — and prints no excerpt, because the excerpt would be the name
      // again and two printings read as two different facts.
      const rows = [...root.querySelectorAll("li")];
      expect(rows[0].querySelectorAll("mark")).toHaveLength(1);
      expect(rows[0].querySelector("mark")?.textContent).toBe("Alpha");
      expect(rows[0].querySelector(".query-list-text")).toBeNull();
      // The content-matched page has no name evidence, so its name is unmarked
      // and the matching block's text is what carries the highlight.
      expect(rows[1].querySelector(".query-page-name")?.querySelector("mark")).toBeNull();
      expect(rows[1].querySelector(".query-list-text")?.textContent).toBe("a body line that mentions alpha");
      expect(rows[1].querySelector(".query-list-text mark")?.textContent).toBe("alpha");
    } finally {
      dispose();
    }
  });

  it("q3_page_rows_never_fabricate_properties_for_a_virtual_suggestion", () => {
    const virtual: QueryPageHit = {
      entity: "page",
      page: { name: "Unwritten", kind: "page", date_key: null, path: "" },
      display_text: "Unwritten",
      evidence: [],
      score: 1,
    };
    const { root, dispose } = mount(() => (
      <QueryPageResults hits={() => [virtual]} view={() => ({ view: "table", columns: ["status"] })} onOpen={() => {}} />
    ));
    try {
      // No `row`, so no properties: the cell is empty rather than inventing a
      // value for a page that does not exist yet.
      expect(root.querySelector("tbody tr td:last-child")?.textContent).toBe("");
    } finally {
      dispose();
    }
  });
});
