import { describe, it, expect } from "vitest";
import { quoteEdnString, unquoteEdnString } from "./edn";

// The `{{query …}}` EXTENT cases that used to live here moved with the reader
// itself, to `src/editor/queryMacro.test.ts` — where they are now asserted
// against the SHARED corpus that `crates/tine-core/tests/query_macro_extents.rs`
// also reads, so the two readers cannot drift (SPEC §4.3.1, §7.9). They were
// deleted from here rather than duplicated: two copies of a corpus is the same
// defect as two copies of a reader.
//
// The `splitTrailingMap` cases went the same way for a stronger reason: the
// reader itself is gone (I-12). "Where does the trailing options map begin" is
// answered once, in Rust, and `crates/tine-core/src/query/macro_text.rs` already
// asserts every case this file used to — braces inside an EDN string, a `}`
// inside a `[[page ref]]`, a form with no trailing map — plus the two this
// reader could not have passed: the advanced whole-map form, and a `}` inside a
// TQL `'…'` literal.

describe("edn helpers", () => {
  it("quote/unquote round-trips quotes and backslashes", () => {
    for (const s of ["plain", 'a "b" c', "back\\slash", 'mix "x"\\y']) {
      expect(unquoteEdnString(quoteEdnString(s))).toBe(s);
    }
  });

});
