import { afterEach, describe, expect, it, vi } from "vitest";
import { languageDisplayName } from "./spellcheckSettings";

describe("languageDisplayName", () => {
  afterEach(() => vi.unstubAllGlobals());

  // GH #580: dialect names ("Mexican Spanish") sorted regional variants apart
  // from their language; the standard form keeps them adjacent.
  it("names regional variants as Language (Region) so they sort together", () => {
    vi.stubGlobal("navigator", { language: "en" });
    expect(languageDisplayName("en_US")).toBe("English (United States)");
    expect(languageDisplayName("es_MX")).toBe("Spanish (Mexico)");
    expect(languageDisplayName("es_PE")).toBe("Spanish (Peru)");
    expect(languageDisplayName("cs_CZ")).toBe("Czech (Czechia)");
  });
});
