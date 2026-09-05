// How the ONE engine reads a macro argument — stated explicitly, for tests.
//
// The frontend no longer decides where a trailing options map begins, nor
// whether a `{{query …}}` holds the legacy DSL or advanced datalog. `query_parse`
// answers both, in Rust (SPEC §7.1, X4; I-12). `src/mock.ts` deliberately cannot
// answer them — it has no parser, and a copy of one is exactly the twin this
// packet deleted — so it retains the whole argument verbatim as `source.original`
// with no options.
//
// That is the honest behaviour of a backend with no engine, but it means a test
// that depends on either reading has to SAY what the engine answers, instead of
// relying on a frontend regex it can no longer reach. This helper is that
// statement: a table from macro argument to the reading, with no parsing of any
// kind. An argument the table does not mention is a test asking for a reading it
// never declared, and it throws rather than guessing.
import { vi } from "vitest";
import { backend } from "./backend";
import type { ParsedQuery, Query, Source, ViewSettings } from "./editor/queryIr";

/** One declared reading: the form, the opaque options map, and which source
 *  variant the engine recognised. */
export interface QueryReading {
  /** `source.original` — the authored form WITHOUT the options map. */
  form: string;
  /** `source.og_options` — the opaque map INCLUDING its braces, or "". */
  opts?: string;
  /** The source variant. `og` unless stated; `advanced` is what makes the
   *  datalog path run. */
  kind?: Source["kind"];
  /** The view settings the engine lifted out of the text and the block's
   *  `tine.*` properties. */
  view?: ViewSettings;
}

function readingToIr(reading: QueryReading): ParsedQuery {
  const kind = reading.kind ?? "og";
  const original = reading.form;
  const og_options = reading.opts ?? "";
  const source: Source =
    kind === "builder"
      ? { kind: "builder" }
      : kind === "advanced"
        ? { kind: "advanced", original, og_options }
        : kind === "tql"
          ? { kind: "tql", original, og_options }
          : { kind: "og", original, og_options };
  // A stub does not invent a filter: `raw` is the IR's own spelling for text the
  // reader retained but did not interpret (§4.3.2), and it carries the payload
  // losslessly so a `preserveForm` print can still re-emit it. Tests here are
  // about the READING, never about what the filter means.
  const query: Query = {
    anchor: "block",
    filter: { kind: "raw", text: original, diagnostic_kind: "not_applicable" },
    diagnostics: [],
    source,
  };
  return { query, view: reading.view ?? {} };
}

/** Install a `query_parse` stub that answers only for the arguments listed.
 *
 *  Keys are the macro argument as the component receives it — the macro body
 *  with the name already stripped. */
export function backendReadsQueries(readings: Record<string, QueryReading>): void {
  vi.spyOn(backend(), "parseQuery").mockImplementation(async (text: string) => {
    const reading = readings[text];
    if (!reading) {
      throw new Error(
        `this test did not declare how the engine reads ${JSON.stringify(text)} — `
          + "add it to backendReadsQueries({...}) rather than expecting the mock to parse it",
      );
    }
    return readingToIr(reading);
  });
}
