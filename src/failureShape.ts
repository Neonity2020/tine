// The one producer of "how do we name a caught failure in an always-on log?"
// (I-12). Every `console.*` site that used to hand the raw `error` straight to
// the console passes it through here instead.
//
// Why (I-5): a caught error is not content-free. A rejected Tauri command
// carries the Rust error string, and those embed the thing the operation was
// about — a page name, a graph path, the TeX that KaTeX refused, the PDF a
// renderer could not draw. Handing that error straight to `console.error` is
// always-on: no flag gates it, and the WebView inspector ships in release builds
// (`src-tauri/Cargo.toml`, feature `devtools`), so the message is one panel
// away on a user's machine and one paste away from a public issue.
//
// Why not just delete the argument (I-9): a failure the user can see — a
// sticky conflict banner, a print that did not happen — has to stay
// identifiable in the always-on record. So keep the parts that cannot carry
// content: the failure's TYPE, the SIZE of its message, and a stable HASH of
// it. Two reports of the same failure hash alike, a changed failure does not,
// and neither reveals a byte of the message.
//
// Exemplars this follows: `devtools/lsdoc-diff/diagnostic.ts`
// (`parserDiagnostic` — offset/bytes/hash instead of the parser input) and
// `update.ts` (`safeUpdaterErrorChain` — classified stages instead of prose).

export interface FailureShape {
  /** The failure's type as a code identifier — never a message. */
  kind: string;
  /** UTF-8 length of the message this shape replaced. */
  bytes: number;
  /** 16 hex chars over the message: stable, one-way, content-free. */
  hash: string;
}

const encoder = new TextEncoder();

/** A code identifier, so a `kind` can never smuggle a message through a class
 *  name or a writable `Error.name`. */
const IDENTIFIER = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

/** The one hash used by every content-free diagnostic shape (I-12): a 64-bit
 *  FNV-1a pair rendered as 16 hex chars. Not a security primitive — it exists
 *  so two occurrences of one failure are recognisably the same. */
export function hash64(text: string): string {
  let left = 0x811c9dc5;
  let right = 0x9e3779b9;
  for (const byte of encoder.encode(text)) {
    left = Math.imul(left ^ byte, 0x01000193) >>> 0;
    right = Math.imul(right ^ byte, 0x85ebca6b) >>> 0;
  }
  return `${left.toString(16).padStart(8, "0")}${right.toString(16).padStart(8, "0")}`;
}

function renderedMessage(error: unknown): string {
  try {
    if (error instanceof Error) return error.message;
    return String(error);
  } catch {
    return "";
  }
}

function kindOf(error: unknown): string {
  if (error === null) return "null";
  if (typeof error !== "object") return typeof error;
  let name: unknown;
  try {
    name = (error as { constructor?: { name?: unknown } }).constructor?.name;
  } catch {
    name = undefined;
  }
  return typeof name === "string" && IDENTIFIER.test(name) ? name : "object";
}

/** Fixed-shape identity for a caught failure. It deliberately returns no
 *  free-form field, so no call site can log the message by accident. */
export function failureShape(error: unknown): FailureShape {
  const message = renderedMessage(error);
  return { kind: kindOf(error), bytes: encoder.encode(message).length, hash: hash64(message) };
}
