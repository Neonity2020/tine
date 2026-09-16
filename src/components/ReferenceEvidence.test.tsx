import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { OccurrenceControls, ReferenceExcerptBlocks, buildFullMarkedSegments } from "./ReferenceEvidence";
import type { BlockDto, ReferenceBlockEvidence, ReferenceOccurrence } from "../types";

afterEach(() => {
  document.body.innerHTML = "";
});

const occ = (start: number): ReferenceOccurrence => ({
  matched_name: "Books",
  canonical: "books",
  kind: "explicit",
  span: { start, end: start + 5 },
  rule: "page-ref",
});
const evidence = (n: number): ReferenceBlockEvidence => ({
  block_id: "b1",
  occurrences: Array.from({ length: n }, (_, i) => occ(i * 10)),
});

describe("OccurrenceControls (GH #200: no redundant count/jump for a single mention)", () => {
  it("renders nothing when a block mentions the page once", () => {
    const host = document.createElement("div");
    render(() => <OccurrenceControls evidence={evidence(1)} onOccurrence={() => {}} />, host);
    expect(host.querySelector(".reference-occurrence-controls")).toBeNull();
    expect(host.textContent ?? "").not.toContain("mention");
  });

  it("shows the count and one jump button per occurrence when a block mentions the page 2+ times", () => {
    const host = document.createElement("div");
    render(() => <OccurrenceControls evidence={evidence(3)} onOccurrence={() => {}} />, host);
    expect(host.querySelector(".reference-occurrence-controls")).not.toBeNull();
    expect(host.textContent ?? "").toContain("3 mentions");
    expect(host.querySelectorAll(".reference-occurrence-jump").length).toBe(3);
  });
});

const WORD = "Books";

function blockWith(mentions: number, gap: number): { block: BlockDto; evidence: ReferenceBlockEvidence } {
  const filler = "x".repeat(gap);
  const raw = Array.from({ length: mentions }, () => `${WORD}${filler}`).join("");
  const occurrences: ReferenceOccurrence[] = Array.from({ length: mentions }, (_, index) => ({
    matched_name: WORD,
    canonical: "books",
    kind: "plain",
    span: { start: index * (WORD.length + gap), end: index * (WORD.length + gap) + WORD.length },
    rule: "plain_unicode_boundary",
  }));
  return {
    block: { id: "b1", raw, children: [] } as unknown as BlockDto,
    evidence: { block_id: "b1", occurrences, total: mentions },
  };
}

function renderExcerpt(mentions: number, gap: number) {
  const { block, evidence: item } = blockWith(mentions, gap);
  const host = document.createElement("div");
  document.body.appendChild(host);
  render(
    () => (
      <ReferenceExcerptBlocks blocks={[block]} evidence={[item]} page="Journal" kind="journal" />
    ),
    host,
  );
  return host;
}

describe("Unlinked reference occurrences (GH #200 round 2: the highlight is the affordance)", () => {
  it("retires the numbered jump row when the excerpt already shows every mention", () => {
    // The reporter's case: a SHORT block with four mentions rendered four marks
    // AND four numbered circles saying the same thing twice.
    const host = renderExcerpt(4, 4);
    expect(host.querySelectorAll(".reference-excerpt-mark").length).toBe(4);
    expect(host.querySelector(".reference-occurrence-controls")).toBeNull();
    expect(host.textContent ?? "").not.toContain("4 mentions");
  });

  it("keeps the jump row when mentions fall outside the excerpt windows", () => {
    // The long-block case the row exists for: the excerpt caps at three windows,
    // so later mentions are unreachable without it.
    const host = renderExcerpt(6, 400);
    const marks = host.querySelectorAll(".reference-excerpt-mark").length;
    expect(marks).toBeLessThan(6);
    expect(host.querySelector(".reference-occurrence-controls")).not.toBeNull();
    expect(host.textContent ?? "").toContain("6 mentions");
  });

  it("marks every mention once the block is expanded, and retires the row with it", () => {
    const host = renderExcerpt(6, 400);
    const expand = [...host.querySelectorAll<HTMLButtonElement>(".reference-show-full")][0];
    expand.click();
    expect(host.querySelectorAll(".reference-excerpt-mark").length).toBe(6);
    expect(host.querySelector(".reference-occurrence-controls")).toBeNull();
  });

  it("marks every span over the whole block, in order, with no window", () => {
    const segments = buildFullMarkedSegments("aBooksbBooksc", [
      { start: 1, end: 6 },
      { start: 7, end: 12 },
    ]);
    expect(segments.map((segment) => segment.text)).toEqual(["a", "Books", "b", "Books", "c"]);
    expect(segments.filter((segment) => segment.marked).length).toBe(2);
    expect(segments.map((segment) => segment.text).join("")).toBe("aBooksbBooksc");
  });
});
