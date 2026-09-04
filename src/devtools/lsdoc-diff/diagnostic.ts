import { hash64 } from "../../failureShape";

export interface ParserDiagnostic {
  offset: number | null;
  inputBytes: number;
  inputHash: string;
}

const encoder = new TextEncoder();

/** Fixed-shape parser-failure identity. It deliberately accepts no Error or
 * free-form detail, so graph text cannot cross the worker/report boundary.
 * The hash comes from the one content-free-diagnostic producer (I-12,
 * `src/failureShape.ts`), not from a second copy of it. */
export function parserDiagnostic(input: string, offset: number | null = null): ParserDiagnostic {
  return { offset, inputBytes: encoder.encode(input).length, inputHash: hash64(input) };
}
