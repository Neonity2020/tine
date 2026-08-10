import { beforeEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { journalTitle } from "./journal";
import {
  doc,
  editorActivationFor,
  emptyPage,
  ensurePageLoaded,
  loadFeed,
  loadSingle,
  loadRoutedPage,
  pageToDto,
  pageByName,
  resetStore,
  restoreTodayJournalInFeed,
  setEditorActivation,
  takeEditorLease,
  reloadHlsIfLoaded,
  reloadPage,
} from "./store";
import { flushPage, markDirty, saveBaselineFor } from "./persistence";
import type { EditorActivationHandle, PageDto } from "./types";

const page = (name: string, path: string, raw: string, rev = `rev-${raw}`): PageDto => ({
  name,
  kind: "page",
  title: name,
  pre_block: null,
  blocks: [{ id: `${name}-${raw}`, raw, collapsed: false, children: [] }],
  format: "md",
  path,
  rev,
});

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
};

const tick = () => new Promise<void>((resolve) => queueMicrotask(resolve));

describe("GH #254 increment 3 activation neighbours", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    resetStore();
  });

  it("retires a raced replacement activation without touching the incumbent", async () => {
    vi.spyOn(backend(), "activateEditor").mockResolvedValueOnce({
      activation: 10,
      target: "pages/Note.md",
      prospective: false,
    });
    await ensurePageLoaded(page("Note", "pages/Note.md", "incumbent"));
    setEditorActivation("Note", 10);

    const next = deferred<EditorActivationHandle>();
    const activate = vi.spyOn(backend(), "activateEditor").mockReturnValueOnce(next.promise);
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    const installing = ensurePageLoaded(page("Note", "pages/other/Note.md", "replacement"));

    await tick();
    expect(activate).toHaveBeenCalledWith("pages/other/Note.md", "replace");
    expect(pageByName("Note")?.path).toBe("pages/Note.md");

    markDirty("Note");
    next.resolve({ activation: 20, target: "pages/other/Note.md", prospective: false });
    expect(await installing).toEqual({ reason: "unsaved-changes", page: "Note" });
    expect(pageByName("Note")?.path).toBe("pages/Note.md");
    expect(editorActivationFor("Note")).toBe(10);
    expect(retire).toHaveBeenCalledWith("pages/other/Note.md", 20);
    expect(retire).not.toHaveBeenCalledWith("pages/Note.md", 10);
  });

  it("installs B, records it, then compare-retires the exact A", async () => {
    vi.spyOn(backend(), "activateEditor")
      .mockResolvedValueOnce({ activation: 11, target: "pages/Note.md", prospective: false })
      .mockResolvedValueOnce({ activation: 12, target: "pages/other/Note.md", prospective: false });
    await ensurePageLoaded(page("Note", "pages/Note.md", "incumbent"));
    setEditorActivation("Note", 11);
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);

    expect(await ensurePageLoaded(page("Note", "pages/other/Note.md", "replacement"))).toBeNull();

    expect(pageByName("Note")?.path).toBe("pages/other/Note.md");
    expect(editorActivationFor("Note")).toBe(12);
    expect(retire).toHaveBeenCalledWith("pages/Note.md", 11);
    expect(retire).not.toHaveBeenCalledWith("pages/other/Note.md", 12);
  });

  it("records a first-create save activation only on the exact issuing instance", async () => {
    const first = emptyPage("New", "page");
    vi.spyOn(backend(), "activateAbsentEditor").mockResolvedValue({
      activation: 31,
      target: "pages/New.md",
      prospective: true,
    });
    await loadRoutedPage(first);
    markDirty("New");
    vi.spyOn(backend(), "savePage").mockResolvedValue({
      revision: "rev-created",
      activation: { activation: 32, target: "pages/New.md", prospective: false },
    } as never);

    expect(await flushPage("New")).toBe(true);
    expect(editorActivationFor("New")).toBe(32);

    resetStore();
    const pending = deferred<unknown>();
    vi.spyOn(backend(), "activateAbsentEditor").mockResolvedValue({
      activation: 41,
      target: "pages/Stale.md",
      prospective: true,
    });
    await loadRoutedPage(emptyPage("Stale", "page"));
    markDirty("Stale");
    vi.spyOn(backend(), "savePage").mockReturnValueOnce(pending.promise as never);
    const saving = flushPage("Stale");
    await tick();

    resetStore();
    const retireStale = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    vi.spyOn(backend(), "activateEditor").mockResolvedValue({
      activation: 50,
      target: "pages/Stale.md",
      prospective: false,
    });
    await loadRoutedPage(page("Stale", "pages/Stale.md", "replacement"));
    pending.resolve({
      revision: "rev-stale-success",
      activation: { activation: 42, target: "pages/Stale.md", prospective: false },
    });
    await saving;
    expect(editorActivationFor("Stale")).toBe(50);
    expect(retireStale).toHaveBeenCalledWith("pages/Stale.md", 42);
  });

  it("abandons and retires save-time activation when its editor is replaced", async () => {
    loadSingle(page("Save race", "pages/Save race.md", "old"));
    markDirty("Save race");

    const acquiring = deferred<EditorActivationHandle>();
    const activate = vi.spyOn(backend(), "activateEditor")
      .mockReturnValueOnce(acquiring.promise)
      .mockResolvedValueOnce({
        activation: 102,
        target: "pages/Save race.md",
        prospective: false,
      });
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    const save = vi.spyOn(backend(), "savePage");

    const saving = flushPage("Save race");
    await vi.waitFor(() => expect(activate).toHaveBeenCalledWith(
      "pages/Save race.md",
      "replace",
    ));

    const replacement = reloadPage(page(
      "Save race",
      "pages/Save race.md",
      "replacement",
      "rev-replacement",
    ));
    await vi.waitFor(() => expect(activate).toHaveBeenCalledWith(
      "pages/Save race.md",
      "replace",
    ));
    expect(await replacement).toBeNull();

    acquiring.resolve({
      activation: 101,
      target: "pages/Save race.md",
      prospective: false,
    });
    expect(await saving).toBe(false);
    expect(retire).toHaveBeenCalledWith("pages/Save race.md", 101);
    expect(editorActivationFor("Save race")).toBe(102);
    expect(save).not.toHaveBeenCalled();
  });

  it("keeps managed revision-only save responses compatible", async () => {
    vi.spyOn(backend(), "activateAbsentEditor").mockResolvedValue(null);
    await loadRoutedPage(emptyPage("Managed", "page"));
    markDirty("Managed");
    vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "managed-revision" });

    expect(await flushPage("Managed")).toBe(true);
    expect(editorActivationFor("Managed")).toBeUndefined();
  });

  it("does not let a stale save success update its replacement instance", async () => {
    vi.spyOn(backend(), "activateEditor")
      .mockResolvedValueOnce({
        activation: 111,
        target: "pages/Late.md",
        prospective: false,
      })
      .mockResolvedValueOnce({
        activation: 112,
        target: "pages/Late.md",
        prospective: false,
      });
    await loadRoutedPage(page("Late", "pages/Late.md", "old", "rev-old"));
    markDirty("Late");

    const pending = deferred<unknown>();
    const save = vi.spyOn(backend(), "savePage").mockReturnValueOnce(pending.promise as never);
    const saving = flushPage("Late");
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(1));

    expect(await reloadPage(page(
      "Late",
      "pages/Late.md",
      "replacement",
      "rev-replacement",
    ))).toBeNull();
    pending.resolve({ revision: "rev-stale-save" });

    expect(await saving).toBe(false);
    expect(saveBaselineFor("Late")).toBe("rev-replacement");
    expect(editorActivationFor("Late")).toBe(112);
  });

  it("publishes present feed DTOs only after activation and installation", async () => {
    const activation = deferred<EditorActivationHandle>();
    const activate = vi.spyOn(backend(), "activateEditor").mockReturnValue(activation.promise);
    const loading = loadFeed([page("Feed", "journals/Feed.md", "feed")]);

    await tick();
    expect(activate).toHaveBeenCalledWith("journals/Feed.md", "replace");
    expect(doc.feed).not.toContain("Feed");
    activation.resolve({ activation: 60, target: "journals/Feed.md", prospective: false });
    await loading;
    expect(doc.feed).toEqual(["Feed"]);
    expect(editorActivationFor("Feed")).toBe(60);

    resetStore();
    vi.spyOn(backend(), "activateEditor").mockRejectedValueOnce(new Error("activation failed"));
    await loadFeed([page("Declined", "journals/Declined.md", "declined")]);
    expect(pageByName("Declined")).toBeUndefined();
    expect(doc.feed).not.toContain("Declined");
  });

  it("activates absent today placeholders before publishing both installation paths", async () => {
    const today = journalTitle(new Date());
    const first = deferred<EditorActivationHandle>();
    const activate = vi.spyOn(backend(), "activateAbsentEditor").mockReturnValueOnce(first.promise);
    const loading = loadFeed([emptyPage(today, "journal")]);
    await tick();
    expect(activate).toHaveBeenCalledWith(today, "journal");
    expect(doc.feed).not.toContain(today);
    first.resolve({ activation: 70, target: `journals/${today}.md`, prospective: true });
    await loading;
    expect(editorActivationFor(today)).toBe(70);
    expect(pageToDto(today)?.path).toBe(`journals/${today}.md`);
    expect(doc.feed[0]).toBe(today);

    resetStore();
    const second = deferred<EditorActivationHandle>();
    activate.mockReturnValueOnce(second.promise);
    const restoring = restoreTodayJournalInFeed();
    await tick();
    expect(doc.feed).not.toContain(today);
    second.resolve({ activation: 71, target: `journals/${today}.md`, prospective: true });
    await restoring;
    expect(editorActivationFor(today)).toBe(71);
    expect(pageToDto(today)?.path).toBe(`journals/${today}.md`);
    expect(doc.feed[0]).toBe(today);
  });

  it("retries PDF notes refresh after a lease and uses same-path replace activation", async () => {
    vi.spyOn(backend(), "activateEditor").mockResolvedValueOnce({
      activation: 80,
      target: "pages/hls__doc.md",
      prospective: false,
    });
    await ensurePageLoaded(page("hls__doc", "pages/hls__doc.md", "old"));
    setEditorActivation("hls__doc", 80);
    const release = takeEditorLease("hls__doc");
    const read = vi.spyOn(backend(), "getPage").mockResolvedValue(
      page("hls__doc", "pages/hls__doc.md", "new"),
    );
    const replace = vi.spyOn(backend(), "activateEditor").mockResolvedValueOnce({
      activation: 81,
      target: "pages/hls__doc.md",
      prospective: false,
    });

    await reloadHlsIfLoaded("hls__doc");
    expect(read).not.toHaveBeenCalled();
    release();
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(1));
    await vi.waitFor(() => expect(pageByName("hls__doc")?.roots
      .map((id) => doc.byId[id]?.raw)).toEqual(["new"]));
    expect(replace).toHaveBeenCalledWith("pages/hls__doc.md", "replace");
    expect(editorActivationFor("hls__doc")).toBe(81);
  });
});
