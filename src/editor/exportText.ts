// Configurable text export for a block subtree / multi-block selection — Tine's
// take on OG Logseq's "Copy / Export" modal (handler/export/text.cljs). The core
// is pure (operates on an ExportNode tree) so it's unit-testable; the store
// builds the tree from the live doc. The inline "remove" transforms are
// pragmatic regexes (not a full inline parse) — enough for the common cases the
// modal offers, matching OG's option set within reason.

import { isPropertyLine } from "../render/block";

// dashes  = Logseq outline: `\t`×level + "- " + text (paste back into Logseq).
// spaces  = indentation preserved with spaces, NO bullet (portable indented text).
// no-indent = flat: no indentation, no bullet (plain lines).
export type IndentStyle = "dashes" | "spaces" | "no-indent";

export interface ExportOptions {
  indent: IndentStyle;
  stripLinks: boolean; // [[Foo]] -> Foo
  removeEmphasis: boolean; // **/__/*/_/~~/== markers dropped
  removeTags: boolean; // #tag and #[[tag]] removed
  removeProperties: boolean; // drop `key:: value` lines
  newlineAfterBlock: boolean; // blank line after each block
}

export const DEFAULT_EXPORT_OPTIONS: ExportOptions = {
  indent: "dashes",
  stripLinks: false,
  removeEmphasis: false,
  removeTags: false,
  removeProperties: false,
  newlineAfterBlock: false,
};

export interface ExportNode {
  raw: string;
  children: ExportNode[];
}

/** Apply the inline "remove" transforms to one content line. */
function stripInline(text: string, opts: ExportOptions): string {
  let s = text;
  if (opts.removeTags) {
    s = s.replace(/#\[\[[^\]]*\]\]/g, ""); // #[[Foo Bar]]
    s = s.replace(/(^|\s)#[\w/-]+/g, "$1"); // #tag (keep the boundary char)
    s = s.replace(/[ \t]{2,}/g, " "); // tidy gaps left by removed tags
  }
  if (opts.stripLinks) {
    s = s.replace(/\[\[([^\]]*)\]\]/g, "$1"); // [[Foo]] -> Foo
  }
  if (opts.removeEmphasis) {
    s = s.replace(/(\*\*|__)(.*?)\1/g, "$2"); // bold
    s = s.replace(/(\*|_)(.*?)\1/g, "$2"); // italic
    s = s.replace(/~~(.*?)~~/g, "$1"); // strikethrough
    s = s.replace(/==(.*?)==/g, "$1"); // highlight
  }
  return s;
}

/** Serialize an export-node forest to text per `opts`. */
export function exportOutline(nodes: ExportNode[], opts: ExportOptions): string {
  const out: string[] = [];
  const walk = (n: ExportNode, level: number) => {
    let lines = n.raw.split("\n");
    if (opts.removeProperties) lines = lines.filter((l) => !isPropertyLine(l));
    lines = lines.map((l) => stripInline(l, opts));
    // Trailing blank lines left by stripping add nothing; keep at least one line.
    while (lines.length > 1 && lines[lines.length - 1].trim() === "") lines.pop();
    const first = lines[0] ?? "";

    if (opts.indent === "dashes") {
      const tabs = "\t".repeat(level);
      out.push(`${tabs}- ${first}`.replace(/\s+$/, ""));
      for (const l of lines.slice(1)) out.push(l === "" ? "" : `${tabs}  ${l}`);
    } else if (opts.indent === "spaces") {
      const pad = "  ".repeat(level);
      out.push(`${pad}${first}`.replace(/\s+$/, ""));
      for (const l of lines.slice(1)) out.push(l === "" ? "" : `${pad}${l}`);
    } else {
      out.push(first.replace(/\s+$/, ""));
      for (const l of lines.slice(1)) out.push(l);
    }

    if (opts.newlineAfterBlock) out.push("");
    for (const c of n.children) walk(c, level + 1);
  };
  for (const n of nodes) walk(n, 0);
  while (out.length && out[out.length - 1] === "") out.pop();
  return out.join("\n");
}
