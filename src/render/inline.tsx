// Inline markdown -> Solid components. Produces real interactive DOM (clickable
// [[links]] and #tags), not an innerHTML string. Used to render a block when it
// is not being edited.

import { For, type JSX } from "solid-js";
import { openPage } from "../router";

type Seg =
  | { t: "text"; v: string }
  | { t: "bold"; v: Seg[] }
  | { t: "italic"; v: Seg[] }
  | { t: "strike"; v: Seg[] }
  | { t: "highlight"; v: Seg[] }
  | { t: "code"; v: string }
  | { t: "pageref"; name: string }
  | { t: "tag"; name: string }
  | { t: "blockref"; id: string }
  | { t: "macro"; body: string }
  | { t: "math"; tex: string }
  | { t: "link"; label: string; url: string }
  | { t: "image"; alt: string; url: string };

const TAG_RE = /^#([\w/_-]+)/;

/** Find the index of a closing delimiter `close` at/after `from`. */
function findClose(s: string, from: number, close: string): number {
  const i = s.indexOf(close, from);
  return i;
}

export function parseInline(input: string): Seg[] {
  const out: Seg[] = [];
  let i = 0;
  let plain = "";
  const flush = () => {
    if (plain) {
      out.push({ t: "text", v: plain });
      plain = "";
    }
  };

  while (i < input.length) {
    const rest = input.slice(i);

    // Image: ![alt](url)
    let m = /^!\[([^\]]*)\]\(([^)]+)\)/.exec(rest);
    if (m) {
      flush();
      out.push({ t: "image", alt: m[1], url: m[2] });
      i += m[0].length;
      continue;
    }
    // Link: [label](url)
    m = /^\[([^\]]*)\]\(([^)]+)\)/.exec(rest);
    if (m) {
      flush();
      out.push({ t: "link", label: m[1], url: m[2] });
      i += m[0].length;
      continue;
    }
    // Page ref: [[name]]
    if (rest.startsWith("[[")) {
      const end = findClose(input, i + 2, "]]");
      if (end !== -1) {
        flush();
        out.push({ t: "pageref", name: input.slice(i + 2, end) });
        i = end + 2;
        continue;
      }
    }
    // Block ref: ((id))
    if (rest.startsWith("((")) {
      const end = findClose(input, i + 2, "))");
      if (end !== -1) {
        flush();
        out.push({ t: "blockref", id: input.slice(i + 2, end) });
        i = end + 2;
        continue;
      }
    }
    // Macro: {{ ... }}
    if (rest.startsWith("{{")) {
      const end = findClose(input, i + 2, "}}");
      if (end !== -1) {
        flush();
        out.push({ t: "macro", body: input.slice(i + 2, end).trim() });
        i = end + 2;
        continue;
      }
    }
    // Tag: #name or #[[multi word]]
    if (rest[0] === "#") {
      if (rest.startsWith("#[[")) {
        const end = findClose(input, i + 3, "]]");
        if (end !== -1) {
          flush();
          out.push({ t: "tag", name: input.slice(i + 3, end) });
          i = end + 2;
          continue;
        }
      }
      const tm = TAG_RE.exec(rest);
      if (tm) {
        flush();
        out.push({ t: "tag", name: tm[1] });
        i += tm[0].length;
        continue;
      }
    }
    // Math: $...$ (single-line)
    if (rest[0] === "$") {
      const dbl = rest.startsWith("$$");
      const delim = dbl ? "$$" : "$";
      const end = findClose(input, i + delim.length, delim);
      if (end !== -1 && end > i + delim.length) {
        flush();
        out.push({ t: "math", tex: input.slice(i + delim.length, end) });
        i = end + delim.length;
        continue;
      }
    }
    // Inline code: `...`
    if (rest[0] === "`") {
      const end = findClose(input, i + 1, "`");
      if (end !== -1) {
        flush();
        out.push({ t: "code", v: input.slice(i + 1, end) });
        i = end + 1;
        continue;
      }
    }
    // Emphasis pairs
    const pair = matchPair(rest);
    if (pair) {
      flush();
      out.push({ t: pair.kind, v: parseInline(pair.inner) } as Seg);
      i += pair.len;
      continue;
    }

    plain += input[i];
    i++;
  }
  flush();
  return out;
}

function matchPair(
  rest: string
): { kind: "bold" | "italic" | "strike" | "highlight"; inner: string; len: number } | null {
  const delims: [string, "bold" | "italic" | "strike" | "highlight"][] = [
    ["**", "bold"],
    ["__", "bold"],
    ["~~", "strike"],
    ["==", "highlight"],
    ["*", "italic"],
    ["_", "italic"],
  ];
  for (const [d, kind] of delims) {
    if (rest.startsWith(d)) {
      const end = rest.indexOf(d, d.length);
      if (end !== -1 && end > d.length - 1 && end !== d.length - 1) {
        const inner = rest.slice(d.length, end);
        if (inner.length > 0) return { kind, inner, len: end + d.length };
      }
    }
  }
  return null;
}

function renderSegs(segs: Seg[]): JSX.Element {
  return <For each={segs}>{(s) => renderSeg(s)}</For>;
}

function renderSeg(s: Seg): JSX.Element {
  switch (s.t) {
    case "text":
      return <>{s.v}</>;
    case "bold":
      return <strong>{renderSegs(s.v)}</strong>;
    case "italic":
      return <em>{renderSegs(s.v)}</em>;
    case "strike":
      return <del>{renderSegs(s.v)}</del>;
    case "highlight":
      return <mark>{renderSegs(s.v)}</mark>;
    case "code":
      return <code class="inline-code">{s.v}</code>;
    case "pageref":
      return (
        <a
          class="page-ref"
          onClick={(e) => {
            e.stopPropagation();
            openPage(s.name);
          }}
        >
          <span class="bracket">[[</span>
          {s.name}
          <span class="bracket">]]</span>
        </a>
      );
    case "tag":
      return (
        <a
          class="tag"
          onClick={(e) => {
            e.stopPropagation();
            openPage(s.name);
          }}
        >
          #{s.name}
        </a>
      );
    case "blockref":
      return <span class="block-ref">(({s.id.slice(0, 8)}))</span>;
    case "macro":
      return <span class="macro">{`{{${s.body}}}`}</span>;
    case "math":
      return <span class="math">{s.tex}</span>;
    case "link":
      return (
        <a class="external-link" href={s.url} target="_blank" rel="noreferrer">
          {s.label || s.url}
        </a>
      );
    case "image":
      return <img class="inline-image" src={s.url} alt={s.alt} />;
  }
}

/** Render a block's body text (already stripped of marker/heading prefix). */
export function InlineText(props: { text: string }): JSX.Element {
  return <>{renderSegs(parseInline(props.text))}</>;
}
