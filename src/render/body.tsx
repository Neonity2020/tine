// Block-body rendering: splits a block's text lines into paragraphs, fenced
// code blocks (syntax-highlighted), and markdown tables.

import { For, Show, type JSX } from "solid-js";
import hljs from "highlight.js/lib/common";
import { InlineText } from "./inline";

type BodySeg =
  | { kind: "lines"; lines: string[] }
  | { kind: "code"; lang: string; code: string }
  | { kind: "table"; rows: string[][] };

function isTableRow(line: string): boolean {
  return /^\s*\|.*\|\s*$/.test(line);
}
function isTableSep(line: string): boolean {
  return /^\s*\|?[\s:|-]+\|?\s*$/.test(line) && line.includes("-");
}

export function segmentBody(lines: string[]): BodySeg[] {
  const segs: BodySeg[] = [];
  let buf: string[] = [];
  const flush = () => {
    if (buf.length) segs.push({ kind: "lines", lines: buf });
    buf = [];
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = /^```(\S*)\s*$/.exec(line.trim());
    if (fence) {
      flush();
      const lang = fence[1] || "";
      const code: string[] = [];
      i++;
      while (i < lines.length && !/^```\s*$/.test(lines[i].trim())) {
        code.push(lines[i]);
        i++;
      }
      segs.push({ kind: "code", lang, code: code.join("\n") });
      continue;
    }
    // table: a run of pipe rows
    if (isTableRow(line) && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      flush();
      const rows: string[][] = [];
      const header = splitRow(line);
      i++; // skip separator
      const body: string[][] = [];
      while (i + 1 < lines.length && isTableRow(lines[i + 1])) {
        body.push(splitRow(lines[i + 1]));
        i++;
      }
      rows.push(header, ...body);
      segs.push({ kind: "table", rows });
      continue;
    }
    buf.push(line);
  }
  flush();
  return segs;
}

function splitRow(line: string): string[] {
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((c) => c.trim());
}

function highlight(code: string, lang: string): string {
  try {
    if (lang && hljs.getLanguage(lang)) return hljs.highlight(code, { language: lang }).value;
    return hljs.highlightAuto(code).value;
  } catch {
    return code.replace(/&/g, "&amp;").replace(/</g, "&lt;");
  }
}

export function BodyContent(props: { lines: string[] }): JSX.Element {
  return (
    <For each={segmentBody(props.lines)}>
      {(seg) => {
        if (seg.kind === "code") {
          return (
            <pre class="code-block">
              <code class="hljs" innerHTML={highlight(seg.code, seg.lang)} />
            </pre>
          );
        }
        if (seg.kind === "table") {
          const [head, ...body] = seg.rows;
          return (
            <table class="md-table">
              <thead>
                <tr>
                  <For each={head}>{(c) => <th><InlineText text={c} /></th>}</For>
                </tr>
              </thead>
              <tbody>
                <For each={body}>
                  {(row) => (
                    <tr>
                      <For each={row}>{(c) => <td><InlineText text={c} /></td>}</For>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          );
        }
        return (
          <For each={seg.lines}>
            {(line, i) => (
              <>
                <Show when={i() > 0}>
                  <br />
                </Show>
                <InlineText text={line} />
              </>
            )}
          </For>
        );
      }}
    </For>
  );
}
