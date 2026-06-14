// Parse pasted text into an outline tree (for paste-as-blocks). Mirrors the
// Rust block parser: `- ` bullets, indentation = nesting (tabs or spaces),
// continuation lines join into the block. Plain multiline text with no bullets
// becomes a flat list of blocks.

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

export function parseOutline(text: string): OutlineNode[] {
  const lines = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n").split("\n");

  // No bullets at all -> each non-empty line is its own flat block.
  if (!lines.some((l) => bullet(l))) {
    return lines
      .map((l) => l.trim())
      .filter((l) => l.length > 0)
      .map((l) => ({ raw: l, children: [] }));
  }

  const roots: OutlineNode[] = [];
  const stack: { col: number; contentStart: number; node: OutlineNode }[] = [];
  for (const line of lines) {
    const b = bullet(line);
    if (b) {
      while (stack.length && stack[stack.length - 1].col >= b.col) stack.pop();
      const node: OutlineNode = { raw: b.content, children: [] };
      if (stack.length) stack[stack.length - 1].node.children.push(node);
      else roots.push(node);
      stack.push({ col: b.col, contentStart: b.col + 2, node });
    } else if (stack.length && line.trim().length > 0) {
      const top = stack[stack.length - 1];
      top.node.raw += "\n" + stripWs(line, top.contentStart);
    }
  }
  return roots;
}
