import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { For, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { initParser } from "../render/parse";
import { loadSingle, pageByName, resetStore } from "../store";
import { startEditing } from "../editorController";
import { setAutoPairing } from "../ui";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

// GH #291 context matrix for `{` / `(` (and the rest of the pair set). The
// reported symptom was an *inconsistency*: one reporter saw pairing, another
// did not. That is the (previously opt-in, OFF-by-default) shared pairing
// decision, not per-character behavior: all pair chars ride one engine. The
// matrix pins the decision's every dimension so any future regression names a
// context, never a single character.

beforeAll(() => initParser());

afterEach(() => {
  setAutoPairing(true);
  resetStore();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element) {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

let seq = 0;
function mountEditor(raw: string, format: "md" | "org" = "md") {
  const block: BlockDto = { id: `mx-${++seq}`, raw, collapsed: false, children: [] };
  const name = `MX${seq}`;
  const page: PageDto = { name, kind: "page", title: name, pre_block: null, blocks: [block], format };
  loadSingle(page);
  startEditing(block.id, raw.length);
  const mounted = mount(() => (
    <For each={pageByName(name)?.roots ?? []}>{(id) => <Block id={id} />}</For>
  ));
  return { ...mounted, textarea: mounted.root.querySelector("textarea.block-editor") as HTMLTextAreaElement };
}

/** Simulate a single keystroke the way the browser delivers it: the char lands
 *  in the DOM first, then onInput post-processing runs. */
function typeChar(ta: HTMLTextAreaElement, ch: string, opts: { composing?: boolean; caret?: number } = {}) {
  const caret = opts.caret ?? ta.selectionStart;
  // A real keystroke inserts `ch` AT the caret — never replaces a range.
  const nv = ta.value.slice(0, caret) + ch + ta.value.slice(caret);
  ta.value = nv;
  ta.setSelectionRange(caret + 1, caret + 1);
  const ev = new InputEvent("input", {
    bubbles: true,
    inputType: opts.composing ? "insertCompositionText" : "insertText",
    data: ch,
  });
  if (opts.composing) Object.defineProperty(ev, "isComposing", { value: true });
  ta.dispatchEvent(ev);
}

function keydown(ta: HTMLTextAreaElement, init: KeyboardEventInit) {
  const ev = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  ta.dispatchEvent(ev);
  return ev;
}

describe("auto-pair matrix — ON by default (GH #291 OG parity)", () => {
  it("pair decision is on with no persisted preference, and an explicit opt-out persists", async () => {
    // Storage-level default rule, validated against fresh module instances so
    // the signal genuinely re-initializes from the key.
    localStorage.removeItem("logseq-claude.autopair");
    vi.resetModules();
    expect((await import("../ui")).autoPairing()).toBe(true);
    localStorage.setItem("logseq-claude.autopair", "0");
    vi.resetModules();
    expect((await import("../ui")).autoPairing()).toBe(false);
    localStorage.removeItem("logseq-claude.autopair");
    // The setter itself persists the house encoding: null = on, "0" = explicit off.
    setAutoPairing(true);
    expect(localStorage.getItem("logseq-claude.autopair")).toBeNull();
    setAutoPairing(false);
    expect(localStorage.getItem("logseq-claude.autopair")).toBe("0");
  });

  it.each(["(", "{", "[", "\"", "`"])("inserts the counterpart for %s on an ordinary markdown block", (ch) => {
    const { textarea, dispose } = mountEditor("hello ");
    try {
      typeChar(textarea, ch);
      expect(textarea.value).toBe(`hello ${ch}${({ "(": ")", "{": "}", "[": "]", '"': '"', "`": "`" } as Record<string, string>)[ch]}`);
      expect(textarea.selectionStart).toBe("hello ".length + 1);
    } finally {
      dispose();
    }
  });

  it("in particular: the reported `{` and `(` both pair", () => {
    const { textarea, dispose } = mountEditor("refs: ");
    try {
      typeChar(textarea, "{");
      expect(textarea.value).toBe("refs: {}");
      // Second `{` doubles into the macro form {{}}, caret between.
      typeChar(textarea, "{");
      expect(textarea.value).toBe("refs: {{}}");
      // The same shared decision for `(` / `((` (the doubling paths themselves
      // are unit-covered in autopair.test.ts; this pins the reported surface).
      const b = mountEditor("x ");
      try {
        typeChar(b.textarea, "(");
        expect(b.textarea.value).toBe("x ()");
        typeChar(b.textarea, "(");
        expect(b.textarea.value).toBe("x (())");
      } finally {
        b.dispose();
      }
    } finally {
      dispose();
    }
  });

  it("does not pair mid-word (standard boundary rule), pairs before whitespace/closers", () => {
    const { textarea, dispose } = mountEditor("word");
    try {
      typeChar(textarea, "(", { caret: 0 });
      expect(textarea.value).toBe("(word"); // boundary before a non-space: no closer
      const b = mountEditor("hi ");
      try {
        typeChar(b.textarea, "(");
        expect(b.textarea.value).toBe("hi ()");
      } finally {
        b.dispose();
      }
    } finally {
      dispose();
    }
  });

  it("types through a closer instead of stacking", () => {
    const { textarea, dispose } = mountEditor("");
    try {
      typeChar(textarea, "(");
      expect(textarea.value).toBe("()");
      typeChar(textarea, ")");
      expect(textarea.value).toBe("()");
      expect(textarea.selectionStart).toBe(2);
    } finally {
      dispose();
    }
  });

  it("Backspace between an empty pair deletes both chars", () => {
    const { textarea, dispose } = mountEditor("");
    try {
      typeChar(textarea, "(");
      expect(textarea.value).toBe("()");
      keydown(textarea, { key: "Backspace" });
      expect(textarea.value).toBe("");
    } finally {
      dispose();
    }
  });

  it("pairs inside deliberately literal contexts too (inline code span, fenced code) — the same decision, matching the code-editor parity", () => {
    const { textarea, dispose } = mountEditor("a `code span` end");
    try {
      typeChar(textarea, "(", { caret: "a `code".length });
      expect(textarea.value).toBe("a `code() span` end");
    } finally {
      dispose();
    }
    const fenced = mountEditor("```js\nconst x = 1\n```");
    try {
      // Line-end is a boundary: pairs. (Inside a word it wouldn't — the same
      // shared boundary rule, not a code-fence special case.) A complete fence
      // edits body-only (GH #412/#413), so the typed surface is the payload.
      typeChar(fenced.textarea, "(", { caret: "const x = 1".length });
      expect(fenced.textarea.value).toBe("const x = 1()");
    } finally {
      fenced.dispose();
    }
  });

  it("IME composition does not post-process the pair char; the composed text is left alone", () => {
    const { textarea, dispose } = mountEditor("");
    try {
      textarea.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
      textarea.value = "(";
      textarea.setSelectionRange(1, 1);
      const ev = new InputEvent("input", { bubbles: true, inputType: "insertCompositionText", data: "(" });
      Object.defineProperty(ev, "isComposing", { value: true });
      textarea.dispatchEvent(ev);
      expect(textarea.value).toBe("(");
      textarea.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
    } finally {
      dispose();
    }
  });

  it("selection wrapping stays always-on regardless of the toggle (md + org)", () => {
    setAutoPairing(false);
    const { textarea, dispose } = mountEditor("hello world");
    try {
      textarea.setSelectionRange(0, 5);
      keydown(textarea, { key: "{" });
      expect(textarea.value).toBe("{hello} world");
    } finally {
      dispose();
    }
    const org = mountEditor("hello world", "org");
    try {
      org.textarea.setSelectionRange(0, 5);
      keydown(org.textarea, { key: "/" });
      expect(org.textarea.value).toBe("/hello/ world");
    } finally {
      org.dispose();
    }
  });

  it("`[` still always pairs into `[[` (the always-on OG page-ref path is separate)", () => {
    setAutoPairing(false);
    const { textarea, dispose } = mountEditor("x ");
    try {
      typeChar(textarea, "[");
      expect(textarea.value).toBe("x [");
      typeChar(textarea, "[");
      expect(textarea.value).toBe("x [[]]");
    } finally {
      dispose();
    }
  });

  it("with pairing explicitly OFF, `{` / `(` and friends no longer pair — but everything else about the matrix is unchanged", () => {
    setAutoPairing(false);
    const { textarea, dispose } = mountEditor("text ");
    try {
      typeChar(textarea, "{");
      expect(textarea.value).toBe("text {");
      typeChar(textarea, "(");
      expect(textarea.value).toBe("text {(");
      typeChar(textarea, '"');
      expect(textarea.value).toBe('text {("');
    } finally {
      dispose();
    }
  });

  it("org format: `{` / `(` pair exactly like markdown (shared decision, shared page format path)", () => {
    const { textarea, dispose } = mountEditor("/italic/ ", "org");
    try {
      typeChar(textarea, "{");
      expect(textarea.value).toBe("/italic/ {}");
      // Typing `(` between the braces nests the pair — the shared decision
      // never special-cases the two reported characters.
      typeChar(textarea, "(");
      expect(textarea.value).toBe("/italic/ {()}");
    } finally {
      dispose();
    }
  });
});
