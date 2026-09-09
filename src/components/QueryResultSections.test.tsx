// **The mixed-result DOM contract** (SPEC §7.6, Q3).
//
// A Friendly search answers two questions at once — which PAGES match and which
// BLOCKS match — and before this they arrived in one flat list under one
// presentation. This file is the evidence for the shell that replaced it: two
// named regions, mounted Pages before Blocks, each with its own control slot,
// its own empty state and its own truncation note.
//
// The two facts worth stating twice:
//
//  * **An empty family still draws its controls.** A section that vanished when
//    it had no rows would take the only way to change what it selects with it,
//    and the user would be in a state with no way out (I-10).
//  * **A failed read is not an empty answer.** Nothing about the graph was
//    learned, so "no matching pages" would be a claim the operation never made
//    (I-9). It is an alert, and it replaces the rows rather than joining them.

import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { createSignal, type JSX } from "solid-js";
import { QueryResultSections } from "./QueryResultSections";

afterEach(() => {
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

const section = (root: ParentNode, kind: "page" | "block") =>
  root.querySelector<HTMLElement>(`[data-query-result-kind="${kind}"]`)!;

describe("QueryResultSections", () => {
  it("q3_sections_name_two_regions_pages_before_blocks", () => {
    const { root, dispose } = mount(() => (
      <QueryResultSections
        families={[
          { kind: "page", control: <button type="button">Display pages</button>, empty: () => false, body: () => <p>page rows</p> },
          { kind: "block", control: <button type="button">Display blocks</button>, empty: () => false, body: () => <p>block rows</p> },
        ]}
      />
    ));
    try {
      const sections = [...root.querySelectorAll<HTMLElement>("[data-query-result-kind]")];
      expect(sections.map((element) => element.dataset.queryResultKind)).toEqual(["page", "block"]);
      // Each region is NAMED by its own heading, and the ids are per mount: the
      // same query can be open in two panes, and a block-derived id would make
      // one section's heading label the other's rows.
      for (const element of sections) {
        const labelledBy = element.getAttribute("aria-labelledby")!;
        const heading = [...root.querySelectorAll("h3")].find((h) => h.id === labelledBy)!;
        expect(heading.tagName).toBe("H3");
        expect(element.contains(heading)).toBe(true);
      }
      expect(section(root, "page").querySelector("h3")?.textContent).toBe("Pages");
      expect(section(root, "block").querySelector("h3")?.textContent).toBe("Blocks");
      expect(section(root, "page").textContent).toContain("Display pages");
      expect(section(root, "block").textContent).toContain("Display blocks");
    } finally {
      dispose();
    }
  });

  it("q3_empty_family_keeps_its_controls_and_says_so", () => {
    const { root, dispose } = mount(() => (
      <QueryResultSections
        families={[
          { kind: "page", control: <button type="button">Display pages</button>, empty: () => true, body: () => <p>page rows</p> },
          { kind: "block", control: <button type="button">Display blocks</button>, empty: () => false, body: () => <p>block rows</p> },
        ]}
      />
    ));
    try {
      const pages = section(root, "page");
      expect(pages.textContent).toContain("No matching pages.");
      expect(pages.textContent).not.toContain("page rows");
      // I-10: the control that could change this answer is still reachable.
      expect(pages.querySelector("button")?.textContent).toBe("Display pages");
      expect(section(root, "block").textContent).toContain("block rows");
    } finally {
      dispose();
    }
  });

  it("q3_pending_and_failure_are_not_the_empty_state", () => {
    const [pending, setPending] = createSignal(true);
    const [failure, setFailure] = createSignal<string | null>(null);
    const { root, dispose } = mount(() => (
      <QueryResultSections
        pending={pending}
        pendingMessage={() => "Waiting for the graph…"}
        failure={failure}
        families={[
          { kind: "page", empty: () => true, body: () => <p>page rows</p> },
          { kind: "block", empty: () => true, body: () => <p>block rows</p> },
        ]}
      />
    ));
    try {
      // Busy: the container says so, the text is a live status, and NOTHING
      // claims the graph holds no matches.
      for (const kind of ["page", "block"] as const) {
        expect(section(root, kind).getAttribute("aria-busy")).toBe("true");
        expect(section(root, kind).querySelector('[role="status"]')?.textContent).toBe("Waiting for the graph…");
        expect(section(root, kind).textContent).not.toContain("No matching");
      }

      setPending(false);
      expect(section(root, "page").getAttribute("aria-busy")).toBe("false");
      expect(section(root, "page").textContent).toContain("No matching pages.");

      // A failure REPLACES the empty state: "no matching pages" and "this read
      // did not complete" are different facts, and a user who cannot tell them
      // apart cannot decide what to do.
      setFailure("Search failed: the graph moved");
      expect(section(root, "page").querySelector('[role="alert"]')?.textContent).toBe("Search failed: the graph moved");
      expect(section(root, "page").textContent).not.toContain("No matching pages.");
      expect(section(root, "page").querySelector('[role="status"]')).toBeNull();
    } finally {
      dispose();
    }
  });

  it("q3_truncation_is_the_backends_flag_not_the_rendered_length", () => {
    const [more, setMore] = createSignal(false);
    const { root, dispose } = mount(() => (
      <QueryResultSections
        families={[
          {
            kind: "page",
            empty: () => false,
            hasMore: more,
            countLabel: () => "40 shown",
            body: () => <p>page rows</p>,
          },
          { kind: "block", empty: () => false, body: () => <p>block rows</p> },
        ]}
      />
    ));
    try {
      // The count describes what IS shown and never claims completeness; the
      // truncation note is the backend's own answer, because a section of
      // exactly 40 rows out of 40 matches and one out of 4,000 look identical
      // from here.
      expect(section(root, "page").textContent).toContain("40 shown");
      expect(section(root, "page").textContent).not.toContain("More pages match");
      setMore(true);
      expect(section(root, "page").textContent).toContain("More pages match than are shown.");
      expect(section(root, "block").textContent).not.toContain("More blocks match");
    } finally {
      dispose();
    }
  });
});
