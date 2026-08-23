// GH #357: is the WHOLE visible block text one fenced code block?
// The block editor uses the answer only to stabilize its own visual box
// (same card + font metrics as the rendered code region); the buffer itself
// stays the honest raw text, fences included.

export interface CodeFenceShape {
  /** Info-string language id ("" when none). "calc" is excluded — those
   *  blocks have their own specialized editing mode. */
  lang: string;
}

const MD_FENCE = /^(`{3,}|~{3,})\s*([^\n]*?)\s*$/;
const ORG_SRC = /^#\+begin_src\s+([^\s]+)\s*$/i;
const ORG_EXAMPLE = /^#\+begin_example\s*$/i;
const ORG_END = /^#\+end_(src|example)\s*$/i;

function langOf(info: string): string {
  return info.trim().split(/\s+/)[0]?.toLowerCase() ?? "";
}

const MD_CLOSE = /^(`{3,}|~{3,})$/;

/** Every line after the fence's CLOSER (a run of 3+ fence chars alone, or an
 *  org #+END_ line) is blank, or there is no closer at all — an unclosed fence
 *  still renders as code to end-of-block while the user is typing it. A code
 *  line that merely starts with a fence char (`\`x\``) is never a closer. */
function closesAtEnd(lines: string[], start: number, isClose: (trimmed: string) => boolean): boolean {
  const inner = lines.slice(start);
  const closeIdx = inner.findIndex((line) => isClose(line.trim()));
  if (closeIdx === -1) return true; // unclosed fence: everything so far is code
  return inner.slice(closeIdx + 1).every((line) => line.trim() === "");
}

/** Whole-block fenced-code shape, matching what the renderer puts into a
 *  single code card. Mixed content (a paragraph before/after the fence,
 *  another fence after a closed fence) is NOT code-shaped and returns null,
 *  as is anything whose info string is `calc`. */
export function codeFenceOnly(text: string, format: "md" | "org"): CodeFenceShape | null {
  const lines = text.split("\n");
  const first = lines[0] ?? "";
  if (format === "md") {
    const m = MD_FENCE.exec(first);
    if (!m) return null;
    const lang = langOf(m[2] ?? "");
    if (lang === "calc") return null;
    if (!closesAtEnd(lines, 1, (t) => MD_CLOSE.test(t))) return null;
    return { lang };
  }
  const src = ORG_SRC.exec(first);
  if (src) {
    const lang = langOf(src[1] ?? "");
    if (lang === "calc") return null;
    if (!closesAtEnd(lines, 1, (t) => ORG_END.test(t))) return null;
    return { lang };
  }
  if (ORG_EXAMPLE.test(first)) {
    if (!closesAtEnd(lines, 1, (t) => ORG_END.test(t))) return null;
    return { lang: "" };
  }
  return null;
}
