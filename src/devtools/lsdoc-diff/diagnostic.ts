export interface ParserDiagnostic {
  offset: number | null;
  inputBytes: number;
  inputHash: string;
}

const encoder = new TextEncoder();

/** Fixed-shape parser-failure identity. It deliberately accepts no Error or
 * free-form detail, so graph text cannot cross the worker/report boundary. */
export function parserDiagnostic(input: string, offset: number | null = null): ParserDiagnostic {
  const bytes = encoder.encode(input);
  let left = 0x811c9dc5;
  let right = 0x9e3779b9;
  for (const byte of bytes) {
    left = Math.imul(left ^ byte, 0x01000193) >>> 0;
    right = Math.imul(right ^ byte, 0x85ebca6b) >>> 0;
  }
  return {
    offset,
    inputBytes: bytes.length,
    inputHash: `${left.toString(16).padStart(8, "0")}${right.toString(16).padStart(8, "0")}`,
  };
}
