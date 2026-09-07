// The builder's model, over the IR.
//
// The old file in this place tested a parser and a printer that no longer exist
// here: `parseQuery`/`toDsl` round trips, `clauseToAdvanced` conversions, DSL
// quoting. Those were the frontend's second query language, and the tests were
// its specification — keeping them would have been keeping the twin (I-12, D-14).
// What is testable here now is what the builder actually owns: the IR SHAPE each
// picker emits, the phrase each node reads as, and the tree edits.

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  addChild,
  betweenFilter,
  builderLeafKind,
  builderRoot,
  contentFilter,
  currentAgg,
  currentGroup,
  currentSort,
  escapeLike,
  filterLabel,
  journalFilter,
  namespaceFilter,
  onPageFilter,
  pagePropertyFilter,
  pageRefFilter,
  pageTagsFilter,
  planningFilter,
  plainLikeSubstring,
  priorityFilter,
  operatorsFor,
  propertyFilter,
  removeAt,
  replaceAt,
  searchFilter,
  setOp,
  sortLabel,
  taskFilter,
  unwrapAt,
  withAgg,
  withGroup,
  withSort,
  wrapAt,
} from "./queryBuilder";
import {
  forEachFilter,
  type Cardinality,
  type CmpOp,
  type Filter,
  type ObservedType,
  type ViewSettings,
} from "./queryIr";

const A = pageRefFilter("A");
const B = pageRefFilter("B");
const C = pageRefFilter("C");

// **The shapes are the contract with `crates/tine-core/src/query/og.rs`.**
// A chip added here and the same filter typed as OG text must be the SAME IR, or
// the round trip (add → print → re-parse) silently rewrites the user's query. So
// these assert the exact JSON, each naming the `og.rs` construction it mirrors.
describe("leaf constructors mirror the OG parser's IR", () => {
  it("`[[x]]` / `#x` is `refs any (name = x)` (Filter::page_ref, Q2)", () => {
    expect(pageRefFilter("Foo")).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "refs",
        quant: "any",
        pred: { kind: "leaf", leaf: { kind: "attr", attr: "name", op: "eq", value: { kind: "text", text: "Foo" } } },
      },
    });
  });

  it("`(task …)` is `task in [...]`, and an empty pick is OG's open-task set", () => {
    expect(taskFilter(["TODO", "DOING"])).toEqual({
      kind: "leaf",
      leaf: {
        kind: "attr",
        attr: "task",
        op: "in",
        value: { kind: "list", items: [{ kind: "text", text: "TODO" }, { kind: "text", text: "DOING" }] },
      },
    });
    // og.rs: "OG drops `(task)` with no markers; Tine's shipped behaviour reads
    // it as any open task and the corpus depends on it."
    const empty = taskFilter([]);
    expect(empty.kind === "leaf" && empty.leaf.kind === "attr" && empty.leaf.value).toEqual({
      kind: "list",
      items: ["TODO", "DOING", "NOW", "LATER"].map((text) => ({ kind: "text", text })),
    });
  });

  it("`(priority …)` defaults to A/B/C, matching og.rs", () => {
    const empty = priorityFilter([]);
    expect(empty.kind === "leaf" && empty.leaf.kind === "attr" && empty.leaf.value).toEqual({
      kind: "list",
      items: ["A", "B", "C"].map((text) => ({ kind: "text", text })),
    });
  });

  it("`(property k v)` is `props any (key = k AND value = v)` (og.rs::property_leaf)", () => {
    expect(propertyFilter("type", "book")).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "props",
        quant: "any",
        pred: {
          kind: "and",
          items: [
            { kind: "leaf", leaf: { kind: "attr", attr: "key", op: "eq", value: { kind: "text", text: "type" } } },
            { kind: "leaf", leaf: { kind: "attr", attr: "value", op: "eq", value: { kind: "text", text: "book" } } },
          ],
        },
      },
    });
    // `(property k)` with no value is the bare key test, not `value = ""`.
    expect(propertyFilter("public", null)).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "props",
        quant: "any",
        pred: { kind: "leaf", leaf: { kind: "attr", attr: "key", op: "eq", value: { kind: "text", text: "public" } } },
      },
    });
  });

  it("page-row filters are read THROUGH the page relation (og.rs::through_page)", () => {
    const throughPage = (inner: unknown) => ({
      kind: "leaf",
      leaf: { kind: "rel", rel: "page", quant: "any", pred: inner },
    });
    expect(onPageFilter("Alpha")).toEqual(
      throughPage({ kind: "leaf", leaf: { kind: "attr", attr: "name", op: "eq", value: { kind: "text", text: "Alpha" } } }),
    );
    // OG `(namespace x)` is recursive membership: the name starts with `x/`.
    expect(namespaceFilter("Projects")).toEqual(
      throughPage({ kind: "leaf", leaf: { kind: "attr", attr: "name", op: "starts_with", value: { kind: "text", text: "Projects/" } } }),
    );
    expect(journalFilter()).toEqual(
      throughPage({ kind: "leaf", leaf: { kind: "attr", attr: "journal", op: "eq", value: { kind: "bool", bool: true } } }),
    );
    expect(pagePropertyFilter("fach", null)).toEqual(throughPage(propertyFilter("fach", null)));
  });

  it("`(page-tags …)` is the page's `tags` property with a set test", () => {
    expect(pageTagsFilter(["research"])).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "page",
        quant: "any",
        pred: {
          kind: "leaf",
          leaf: {
            kind: "rel",
            rel: "props",
            quant: "any",
            pred: {
              kind: "and",
              items: [
                { kind: "leaf", leaf: { kind: "attr", attr: "key", op: "eq", value: { kind: "text", text: "tags" } } },
                { kind: "leaf", leaf: { kind: "attr", attr: "value", op: "in", value: { kind: "list", items: [{ kind: "text", text: "research" }] } } },
              ],
            },
          },
        },
      },
    });
  });

  it("`(scheduled)` / `(deadline)` are presence leaves, never a date", () => {
    expect(planningFilter("scheduled")).toEqual({
      kind: "leaf",
      leaf: { kind: "attr", attr: "scheduled", op: "is_set", value: { kind: "none" } },
    });
  });

  // og.rs::bounded — two bounds, one bound, no bounds are three DIFFERENT leaves.
  // The IR keeps the literal unresolved, so a cached query never pins a day.
  it("`(between …)` degrades exactly as og.rs::bounded does", () => {
    expect(betweenFilter("scheduled", "-7d", "+7d")).toEqual({
      kind: "leaf",
      leaf: {
        kind: "attr",
        attr: "scheduled",
        op: "between",
        value: { kind: "list", items: [{ kind: "date", literal: "-7d" }, { kind: "date", literal: "+7d" }] },
      },
    });
    expect(betweenFilter("deadline", "today", "")).toEqual({
      kind: "leaf",
      leaf: { kind: "attr", attr: "deadline", op: "ge", value: { kind: "date", literal: "today" } },
    });
    expect(betweenFilter("deadline", "", "+14d")).toEqual({
      kind: "leaf",
      leaf: { kind: "attr", attr: "deadline", op: "le", value: { kind: "date", literal: "+14d" } },
    });
    expect(betweenFilter("deadline", "", "")).toEqual({
      kind: "leaf",
      leaf: { kind: "attr", attr: "deadline", op: "is_set", value: { kind: "none" } },
    });
  });

  it("`(between any …)` expands to the faithful three-way or", () => {
    const any = betweenFilter("any", "-7d", "+7d");
    expect(any.kind).toBe("or");
    expect(any.kind === "or" && any.items).toEqual([
      betweenFilter("journal", "-7d", "+7d"),
      betweenFilter("scheduled", "-7d", "+7d"),
      betweenFilter("deadline", "-7d", "+7d"),
    ]);
    // The journal form goes through the page; the planning forms do not.
    expect(betweenFilter("journal", "-7d", "+7d")).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "page",
        quant: "any",
        pred: {
          kind: "leaf",
          leaf: {
            kind: "attr",
            attr: "day",
            op: "between",
            value: { kind: "list", items: [{ kind: "date", literal: "-7d" }, { kind: "date", literal: "+7d" }] },
          },
        },
      },
    });
  });
});

// `og::escape_like` and its inverse `og::plain_like_substring`. The builder emits
// the pattern synchronously as the chip commits, so these two are transcribed
// rather than shared — and an unescaped `%` would make `50%` match everything
// after the `50`.
describe("LIKE escaping (transcribed from og::escape_like)", () => {
  it("treats the user's `%`, `_` and `\\` as data", () => {
    expect(escapeLike("50% _ok_ a\\b")).toBe("50\\% \\_ok\\_ a\\\\b");
  });

  it("a content chip carries the `%…%` substring pattern", () => {
    expect(contentFilter("100% done")).toEqual({
      kind: "leaf",
      leaf: { kind: "attr", attr: "content", op: "like", value: { kind: "text", text: "%100\\% done%" } },
    });
  });

  it("round-trips through the printer's inverse", () => {
    for (const text of ["plain", "100% done", "a_b", "back\\slash", "%%%"]) {
      expect(plainLikeSubstring(`%${escapeLike(text)}%`)).toBe(text);
    }
  });

  it("refuses a pattern that is not a plain substring, so the label never lies", () => {
    // An unescaped wildcard in the middle is a real pattern, not a literal.
    expect(plainLikeSubstring("%a%b%")).toBeNull();
    expect(plainLikeSubstring("no percent signs")).toBeNull();
  });
});

describe("builderLeafKind", () => {
  it("recognises every shape a picker can emit", () => {
    expect(builderLeafKind(pageRefFilter("A"))).toBe("page");
    expect(builderLeafKind(taskFilter(["TODO"]))).toBe("task");
    expect(builderLeafKind(priorityFilter(["A"]))).toBe("priority");
    expect(builderLeafKind(propertyFilter("k", "v"))).toBe("property");
    expect(builderLeafKind(propertyFilter("k", null))).toBe("property");
    expect(builderLeafKind(pagePropertyFilter("k", "v"))).toBe("pageProperty");
    expect(builderLeafKind(pageTagsFilter(["t"]))).toBe("pageTags");
    expect(builderLeafKind(planningFilter("scheduled"))).toBe("scheduled");
    expect(builderLeafKind(planningFilter("deadline"))).toBe("deadline");
    expect(builderLeafKind(journalFilter())).toBe("journal");
    expect(builderLeafKind(onPageFilter("P"))).toBe("onPage");
    expect(builderLeafKind(namespaceFilter("N"))).toBe("namespace");
    expect(builderLeafKind(contentFilter("x"))).toBe("content");
    expect(builderLeafKind(searchFilter("a or b"))).toBe("search");
    expect(builderLeafKind(betweenFilter("scheduled", "-7d", "+7d"))).toBe("between");
    expect(builderLeafKind(betweenFilter("journal", "-7d", "+7d"))).toBe("between");
  });

  it("returns null — not a wrong guess — for a shape no picker can re-collect", () => {
    // A typed comparison, a quantifier the pickers do not offer, and a boolean
    // node are all things the builder RENDERS but must not offer to "edit" with a
    // form that would rewrite them into something else.
    const typed: Filter = {
      kind: "leaf",
      leaf: { kind: "attr", attr: "content", op: "gt", value: { kind: "number", number: 3 } },
    };
    expect(builderLeafKind(typed)).toBeNull();
    const everyChild: Filter = {
      kind: "leaf",
      leaf: { kind: "rel", rel: "children", quant: "every", pred: taskFilter(["TODO"]) },
    };
    expect(builderLeafKind(everyChild)).toBeNull();
    expect(builderLeafKind({ kind: "and", items: [] })).toBeNull();
  });
});

// §7.5's anti-Jira property: "every query the parser accepts renders in the
// builder, invalid ones included". The golden wire fixture is the widest IR the
// two sides agree on, so labelling every node of it is the strongest cheap
// statement of totality available here.
describe("filterLabel is total over the IR", () => {
  const fixture = JSON.parse(
    readFileSync(
      join(fileURLToPath(new URL("../..", import.meta.url)), "crates/tine-core/tests/fixtures/query-ir/filter.json"),
      "utf8",
    ),
  ) as Filter;

  it("phrases every node of the golden filter fixture without throwing", () => {
    let nodes = 0;
    forEachFilter(fixture, (node) => {
      nodes += 1;
      const label = filterLabel(node);
      expect(typeof label).toBe("string");
      expect(label.length).toBeGreaterThan(0);
    });
    expect(nodes).toBeGreaterThan(20);
  });

  it("keeps an unparsed span's own text as its phrase (§4.3.2 R4)", () => {
    expect(filterLabel({ kind: "raw", text: "(frobnicate x)", diagnostic_kind: "unknown_head" }))
      .toBe("(frobnicate x)");
  });

  it("marks a disabled subtree rather than hiding it (§3.5, Q12)", () => {
    expect(filterLabel({ kind: "off", inner: pageRefFilter("A") })).toBe("A (off)");
  });

  it("reads the friendly shapes friendlily", () => {
    expect(filterLabel(pageRefFilter("Foo"))).toBe("Foo");
    expect(filterLabel(taskFilter(["NOW", "LATER"]))).toBe("task: NOW | LATER");
    expect(filterLabel(propertyFilter("type", "book"))).toBe("type: book");
    expect(filterLabel(propertyFilter("public", null))).toBe("public: any");
    expect(filterLabel(pagePropertyFilter("fach", "x"))).toBe("page fach: x");
    expect(filterLabel(pageTagsFilter(["a", "b"]))).toBe("page tags: a | b");
    expect(filterLabel(onPageFilter("Alpha"))).toBe("page: Alpha");
    expect(filterLabel(namespaceFilter("Projects"))).toBe("namespace: Projects");
    expect(filterLabel(journalFilter())).toBe("on journal page");
    expect(filterLabel(contentFilter("100% done"))).toBe('text: "100% done"');
    expect(filterLabel(betweenFilter("journal", "-30d", "today"))).toBe("between: -30d ~ today");
    expect(filterLabel(betweenFilter("scheduled", "-7d", "+7d"))).toBe("scheduled between: -7d ~ +7d");
  });
});

describe("builderRoot", () => {
  it("adopts a bare leaf so `+ add filter` has somewhere to add", () => {
    expect(builderRoot(A)).toEqual({ kind: "and", items: [A] });
  });
  it("keeps an `or` root as an `or`, so adding a filter does not change the query", () => {
    expect(builderRoot({ kind: "or", items: [A, B] })).toEqual({ kind: "or", items: [A, B] });
  });
  it("reads `true` as the empty query", () => {
    expect(builderRoot({ kind: "true" })).toEqual({ kind: "and", items: [] });
  });
});

describe("tree edits", () => {
  const root = (items: Filter[]): Filter => ({ kind: "and", items });

  it("addChild appends to the addressed boolean node", () => {
    expect(addChild(root([A]), [], B)).toEqual(root([A, B]));
    expect(addChild(root([{ kind: "or", items: [A] }]), [0], B)).toEqual(root([{ kind: "or", items: [A, B] }]));
  });

  it("addChild refuses a `not`, which holds exactly one child", () => {
    const tree = root([{ kind: "not", inner: A }]);
    expect(addChild(tree, [0], B)).toBe(tree);
  });

  it("removeAt deletes, and prunes the empty node it leaves behind", () => {
    expect(removeAt(root([A, B]), [1])).toEqual(root([A]));
    expect(removeAt(root([A]), [0])).toEqual(root([]));
    // The `or` becomes childless and goes with it, rather than staying as a
    // vacuous `(or)` that would match everything.
    expect(removeAt(root([A, { kind: "or", items: [B] }]), [1, 0])).toEqual(root([A]));
  });

  it("replaceAt swaps one node", () => {
    expect(replaceAt(root([A, B]), [1], taskFilter(["TODO"]))).toEqual(root([A, taskFilter(["TODO"])]));
  });

  it("wrapAt wraps in a new boolean node, `not` included", () => {
    expect(wrapAt(root([A, B]), [1], "or")).toEqual(root([A, { kind: "or", items: [B] }]));
    expect(wrapAt(root([A]), [0], "not")).toEqual(root([{ kind: "not", inner: A }]));
  });

  it("unwrapAt promotes children, through `not` and `off` too", () => {
    expect(unwrapAt(root([A, { kind: "or", items: [B, C] }]), [1])).toEqual(root([A, B, C]));
    expect(unwrapAt(root([{ kind: "not", inner: A }]), [0])).toEqual(root([A]));
    expect(unwrapAt(root([{ kind: "off", inner: A }]), [0])).toEqual(root([A]));
  });

  it("setOp flips the root and any inner node", () => {
    expect(setOp(root([A, B]), [], "or")).toEqual({ kind: "or", items: [A, B] });
    expect(setOp(root([{ kind: "and", items: [A, B] }]), [0], "or")).toEqual(
      root([{ kind: "or", items: [A, B] }]),
    );
  });

  it("a path that no longer addresses anything is a no-op, not a corruption", () => {
    // A popover can outlive the tree it was opened over; an edit at a stale `loc`
    // must leave the query alone rather than delete a neighbour.
    const tree = root([A]);
    expect(removeAt(tree, [7])).toBe(tree);
    expect(replaceAt(tree, [1, 2], B)).toBe(tree);
    expect(wrapAt(tree, [], "or")).toBe(tree);
    expect(unwrapAt(tree, [0])).toBe(tree); // a leaf has no children to promote
    expect(setOp(tree, [0], "or")).toBe(tree);
  });

  it("edits do not mutate the input tree", () => {
    const tree = root([A, { kind: "or", items: [B] }]);
    const before = structuredClone(tree);
    removeAt(tree, [1, 0]);
    addChild(tree, [1], C);
    wrapAt(tree, [0], "not");
    expect(tree).toEqual(before);
  });
});

// Presentation is a separate value (§3.1, Q15). It used to ride in the clause
// tree as fake filter children, which is why wrapping a sort in an OR silently
// disabled it.
describe("view settings edits", () => {
  it("sort holds at most the one key OG can express", () => {
    const view = withSort({}, { field: "priority", dir: "desc" });
    expect(view.sort).toEqual([["priority", "desc"]]);
    expect(currentSort(view)).toEqual({ field: "priority", dir: "desc" });
    expect(currentSort(withSort(view, null))).toBeNull();
    expect(withSort(view, null).sort).toBeUndefined();
  });

  it("a blank field clears rather than writing an empty sort key", () => {
    expect(withSort({}, { field: "   ", dir: "asc" }).sort).toBeUndefined();
  });

  it("`count` is the whole-result count, spelled with the empty field (X3)", () => {
    const view = withAgg({}, { agg: "count", field: null });
    expect(view.aggregates).toEqual([["", "count"]]);
    expect(currentAgg(view)).toEqual({ agg: "count", field: null });
  });

  it("sum/avg keep their field", () => {
    const view = withAgg({}, { agg: "sum", field: "hours" });
    expect(view.aggregates).toEqual([["hours", "sum"]]);
    expect(currentAgg(view)).toEqual({ agg: "sum", field: "hours" });
    expect(currentAgg(withAgg(view, null))).toBeNull();
  });

  it("group-by is independent of the aggregate", () => {
    let view: ViewSettings = withAgg({}, { agg: "count", field: null });
    view = withGroup(view, "page");
    expect(currentGroup(view)).toBe("page");
    expect(currentAgg(view)).toEqual({ agg: "count", field: null });
    expect(currentGroup(withGroup(view, null))).toBeNull();
  });

  it("a view edit does not mutate the view it was given", () => {
    const view: ViewSettings = { sort: [["page", "asc"]] };
    withSort(view, { field: "priority", dir: "desc" });
    withGroup(view, "status");
    expect(view).toEqual({ sort: [["page", "asc"]] });
  });

  it("sortLabel prefers a preset's words", () => {
    expect(sortLabel("modified", "desc")).toBe("newest first");
    expect(sortLabel("rating", "desc")).toBe("rating ↓");
  });
});

// ---------------------------------------------------------------------------
// Typed operators over the registry type (SPEC §7.4, §9 P2; T2)
// ---------------------------------------------------------------------------

describe("operatorsFor: the comparison family for a registry type", () => {
  const ops = (type: ObservedType, cardinality: Cardinality = "one") =>
    operatorsFor({ type, cardinality }).map((o) => o.op);

  /** The operand a row builds for `texts`, or null. */
  const operandOf = (
    type: ObservedType,
    op: CmpOp,
    texts: string[],
    cardinality: Cardinality = "one",
  ) => operatorsFor({ type, cardinality }).find((o) => o.op === op)!.operand(texts);

  it("offers exactly the §9 P2 table, operator for operator", () => {
    expect(ops("number")).toEqual(["eq", "not_eq", "lt", "le", "gt", "ge"]);
    expect(ops("date")).toEqual(["eq", "lt", "gt", "between"]);
    expect(ops("checkbox")).toEqual(["eq", "not_eq"]);
    expect(ops("text")).toEqual(["eq", "not_eq", "like", "starts_with"]);
    expect(ops("ref")).toEqual(["eq", "not_eq"]);
  });

  it("builds the operand KIND the type calls for, not a string for everything", () => {
    expect(operandOf("number", "gt", ["3"])).toEqual({ kind: "number", number: 3 });
    // A6/§4.2.3: the date operand is the unresolved literal. Resolving it here
    // would freeze "today" to the day the chip was added.
    expect(operandOf("date", "lt", ["today"])).toEqual({ kind: "date", literal: "today" });
    expect(operandOf("date", "between", ["today", "+7d"])).toEqual({
      kind: "list",
      items: [
        { kind: "date", literal: "today" },
        { kind: "date", literal: "+7d" },
      ],
    });
    expect(operandOf("checkbox", "eq", ["true"])).toEqual({ kind: "bool", bool: true });
    expect(operandOf("checkbox", "eq", ["no"])).toEqual({ kind: "bool", bool: false });
    expect(operandOf("text", "eq", ["book"])).toEqual({ kind: "text", text: "book" });
    expect(operandOf("ref", "eq", ["Alpha"])).toEqual({ kind: "text", text: "Alpha" });
  });

  it("makes `contains` a like PATTERN whose %, _ and \\ are the user's data", () => {
    expect(operandOf("text", "like", ["50%"])).toEqual({ kind: "text", text: "%50\\%%" });
    expect(operandOf("text", "starts_with", ["Proj"])).toEqual({ kind: "text", text: "Proj" });
  });

  it("refuses text that is not a value of the type, instead of filtering for nothing", () => {
    expect(operandOf("number", "eq", ["not a number"])).toBeNull();
    expect(operandOf("number", "eq", [""])).toBeNull();
    expect(operandOf("checkbox", "eq", ["maybe"])).toBeNull();
    // A half-filled range is not a range.
    expect(operandOf("date", "between", ["today", ""])).toBeNull();
  });

  it("says `contains this value` for a many-valued key, keeping the same operator", () => {
    const many = operatorsFor({ type: "text", cardinality: "many" });
    expect(many.find((o) => o.op === "eq")!.label).toBe("contains this value");
    // `value = 'x'` on a many-valued key ALREADY means "one of its values is x".
    // Only the words were wrong; there is no new operator and no quantifier.
    expect(many.map((o) => o.op)).toEqual(["eq", "not_eq", "like", "starts_with"]);
    expect(operatorsFor({ type: "text", cardinality: "one" }).find((o) => o.op === "eq")!.label)
      .toBe("is");
  });

  it("puts the typed comparison on the VALUE attribute, leaving the key test alone", () => {
    const filter = propertyFilter("cost", { op: "gt", operand: { kind: "number", number: 100 } });
    expect(filter).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "props",
        quant: "any",
        pred: {
          kind: "and",
          items: [
            { kind: "leaf", leaf: { kind: "attr", attr: "key", op: "eq", value: { kind: "text", text: "cost" } } },
            { kind: "leaf", leaf: { kind: "attr", attr: "value", op: "gt", value: { kind: "number", number: 100 } } },
          ],
        },
      },
    });
    expect(pagePropertyFilter("cost", { op: "gt", operand: { kind: "number", number: 100 } })).toEqual({
      kind: "leaf",
      leaf: { kind: "rel", rel: "page", quant: "any", pred: filter },
    });
  });

  it("leaves `(any value)` exactly as it was — every family still offers it", () => {
    // The presence row is NOT one of the typed operators: it asks a different
    // question, and it is the ONE row a key of any type always has.
    expect(propertyFilter("public", null)).toEqual({
      kind: "leaf",
      leaf: {
        kind: "rel",
        rel: "props",
        quant: "any",
        pred: { kind: "leaf", leaf: { kind: "attr", attr: "key", op: "eq", value: { kind: "text", text: "public" } } },
      },
    });
    expect(propertyFilter("public", "")).toEqual(propertyFilter("public", null));
    expect(propertyFilter("type", "book")).toEqual(
      propertyFilter("type", { op: "eq", operand: { kind: "text", text: "book" } }),
    );
    for (const type of ["text", "number", "date", "checkbox", "ref"] as ObservedType[]) {
      expect(operatorsFor({ type, cardinality: "one" }).some((o) => o.op === "is_set")).toBe(false);
    }
  });
});
