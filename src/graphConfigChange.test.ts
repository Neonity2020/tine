// Live `logseq/config.edn` reload, frontend half. See
// docs/contracts/config-live-reload.md §4.
import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { applyConfigDerivedState } from "./graph";
import { favorites, seedFavorites, setFavorites } from "./ui";
import { resetFavoritesLayout } from "./favoritesStore";
import type { GraphMeta } from "./types";

const meta = (over: Partial<GraphMeta> = {}): GraphMeta =>
  ({
    root: "/graph",
    journals_dir: "journals",
    pages_dir: "pages",
    preferred_workflow: "now",
    shortcuts: {},
    start_of_week: 1,
    block_hidden_properties: [],
    default_journal_template: null,
    default_home: null,
    favorites: ["Alpha"],
    favorites_page: null,
    journal_page_title_format: "MMM do, yyyy",
    journal_file_name_format: "yyyy_MM_dd",
    preferred_format: "md",
    macros: {},
    enable_timetracking: true,
    show_brackets: true,
    doc_mode_enter_for_new_block: false,
    logical_outdenting: false,
    logbook_with_second_support: true,
    logbook_enabled_in_timestamped_blocks: true,
    logbook_enabled_in_all_blocks: false,
    guide_announced: false,
    ...over,
  }) as GraphMeta;

afterEach(() => {
  resetFavoritesLayout();
  setFavorites([]);
  vi.restoreAllMocks();
});

describe("applying a live config.edn change", () => {
  it("adopts a favorite added outside Tine", () => {
    seedFavorites(["Alpha"]);
    applyConfigDerivedState(meta({ favorites: ["Alpha", "AddedInLogseq"] }), meta());
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "AddedInLogseq"]);
  });

  it("adopts a favorite removed outside Tine", () => {
    seedFavorites(["Alpha", "Beta"]);
    applyConfigDerivedState(meta({ favorites: ["Beta"] }), meta({ favorites: ["Alpha", "Beta"] }));
    expect(favorites().map((f) => f.name)).toEqual(["Beta"]);
  });

  // A managed graph re-reads configuration after Tine's OWN settings write, so
  // the file moving is not evidence that the user's view is wrong. Re-seeding
  // here would drop the arrangement and re-fetch its page on every star.
  it("does not re-seed favorites the user is already being shown", () => {
    const getPage = vi.spyOn(backend(), "getPage");
    seedFavorites(["Alpha", "Beta"]);
    applyConfigDerivedState(
      meta({ favorites: ["Alpha", "Beta"], favorites_page: "Favorites" }),
      meta({ favorites: ["Zulu"], favorites_page: "Favorites" }),
    );
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta"]);
    expect(getPage).not.toHaveBeenCalled();
  });

  // Everything else on GraphMeta is read reactively from the signal, so it must
  // NOT be re-applied here — a second producer of the same state is the defect
  // this split exists to prevent.
  it("touches nothing when only a reactively-read setting moved", () => {
    const getPage = vi.spyOn(backend(), "getPage");
    seedFavorites(["Alpha"]);
    applyConfigDerivedState(
      meta({ shortcuts: { "editor/undo": "mod+z" }, macros: { hi: "hello" } }),
      meta(),
    );
    expect(favorites().map((f) => f.name)).toEqual(["Alpha"]);
    expect(getPage).not.toHaveBeenCalled();
  });

  it("applies everything on a graph open, where there is no previous meta", () => {
    const getPage = vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    applyConfigDerivedState(meta({ favorites: ["Alpha"], favorites_page: "Favorites" }), null);
    expect(favorites().map((f) => f.name)).toEqual(["Alpha"]);
    expect(getPage).toHaveBeenCalledWith("Favorites", "page");
  });
});
