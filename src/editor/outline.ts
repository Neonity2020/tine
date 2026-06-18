// Parse pasted text into an outline tree (paste-as-blocks). Handles both a
// Logseq outline (every line a `- ` bullet, indentation = nesting, continuation
// lines indented to the bullet's content column) AND arbitrary markdown / plain
// text where headings, paragraphs and `- ` list items are intermixed.
//
// The guiding rule is LOSSLESS: every non-blank line ends up in some block,
// never silently dropped or merged into an unrelated one. A non-bullet line is
// treated as a *continuation* of the preceding bullet only when it is indented
// to at least that bullet's content column with no blank line in between
// (matching how Logseq writes multi-line block bodies); otherwise it becomes
// its own block at its own indentation depth.

export interface OutlineNode {
  raw: string;
  children: OutlineNode[];
}

function leadingWs(line: string): number {
  let i = 0;
  while (i < line.length && (line[i] === " " || line[i] === "\t")) i++;
  return i;
}

function bullet(line: string): { col: number; content: string } | null {
  const col = leadingWs(line);
  const rest = line.slice(col);
  if (rest === "-") return { col, content: "" };
  if (rest.startsWith("- ")) return { col, content: rest.slice(2) };
  return null;
}

function stripWs(line: string, n: number): string {
  let i = 0;
  while (i < n && i < line.length && (line[i] === " " || line[i] === "\t")) i++;
  return line.slice(i);
}

interface Frame {
  col: number; // indentation of this node's marker / first char
  contentStart: number; // column a continuation line must reach to join this node
  kind: "bullet" | "block";
  node: OutlineNode;
}

export function parseOutline(text: string): OutlineNode[] {
  const lines = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n").split("\n");
  const roots: OutlineNode[] = [];
  const stack: Frame[] = [];
  let sawBlank = false;

  // Attach `node` at indentation `col`: pop deeper/equal frames, then nest under
  // the remaining top (or make it a root).
  const place = (col: number, node: OutlineNode) => {
    while (stack.length && stack[stack.length - 1].col >= col) stack.pop();
    if (stack.length) stack[stack.length - 1].node.children.push(node);
    else roots.push(node);
  };

  for (const line of lines) {
    const b = bullet(line);
    if (b) {
      const node: OutlineNode = { raw: b.content, children: [] };
      place(b.col, node);
      stack.push({ col: b.col, contentStart: b.col + 2, kind: "bullet", node });
      sawBlank = false;
      continue;
    }
    if (line.trim().length === 0) {
      sawBlank = true;
      continue;
    }
    const indent = leadingWs(line);
    const top = stack.length ? stack[stack.length - 1] : null;
    // Continuation of the current bullet: indented into its body, no blank gap.
    if (top && top.kind === "bullet" && !sawBlank && indent >= top.contentStart) {
      top.node.raw += "\n" + stripWs(line, top.contentStart);
      continue;
    }
    // Otherwise its own block (heading / paragraph line / loose text). Like a
    // flat plain-text paste, a block never absorbs the lines that follow it.
    const node: OutlineNode = { raw: line.trim(), children: [] };
    place(indent, node);
    stack.push({ col: indent, contentStart: indent, kind: "block", node });
    sawBlank = false;
  }
  return roots;
}
