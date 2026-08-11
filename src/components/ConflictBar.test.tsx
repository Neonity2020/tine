import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { ConflictBar } from "./ConflictBar";
import { Block } from "./Block";
import { PageView } from "./Page";
import { conflicts, clearConflict } from "../ui";
import { backend } from "../backend";
import { loadSingle, resetStore, pageByName, doc, setRaw } from "../store";
import {
  canForceSave,
  flushPage,
  isDirty,
  managedConflictObservationSnapshotFor,
  reobserve,
  shownObservationFor,
} from "../persistence";
import { startEditing } from "../editorController";
import { mainPaneRouter, resetTabsToJournals } from "../router";
import { initParser } from "../render/parse";
import type { PageDto } from "../types";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  for (const name of conflicts()) clearConflict(name);
  resetStore();
  resetTabsToJournals();
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

// Two files can carry the same page NAME — the duplicate-day stray of GH #21,
// or two same-titled pages in different folders. The editor pins such a page to
// its exact `path` so a save writes back to the file it was loaded from.
//
// "Use disk version" has to honour the same pin. Resolving by name reaches the
// backend's CANONICAL owner of that name, so the button silently re-points the
// tab at a different file: the user asked to discard their edits to THIS file
// and instead got someone else's file loaded in its place, with their own
// unsaved work gone. (Direct Files data-safety audit, 2026-08-09, finding 10.)
describe("resolving a conflict on a page pinned to a specific file", () => {
  const sharedName = "2026_06_26";
  const strayPath = "journals/2026_06_26 (1).md";
  const canonicalPath = "journals/2026_06_26.md";

  const page = (path: string, text: string): PageDto => ({
    name: sharedName,
    kind: "journal",
    title: sharedName,
    pre_block: null,
    path,
    rev: `rev-of-${path}:${text}`,
    blocks: [{ id: `block-of-${path}`, raw: text, collapsed: false, children: [], properties: [] }],
  });

  async function mountWithObservedDirectConflict() {
    loadSingle(page(strayPath, "the loaded disk baseline"));
    const loaded = pageByName(sharedName);
    expect(loaded).toBeDefined();
    setRaw(loaded!.roots[0], "the retained local draft");

    // Exercise the real conflict path so the banner has both pieces of Direct
    // authority that ConflictBar presents: an editor activation and the shown
    // observation minted by the refused guarded save.
    const savePage = vi.spyOn(backend(), "savePage")
      .mockRejectedValueOnce(new Error("conflict:41"));
    expect(await flushPage(sharedName)).toBe(false);
    expect(conflicts()).toEqual([sharedName]);
    expect(isDirty(sharedName)).toBe(false);

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <ConflictBar />, root);
    return { root, dispose, savePage };
  }

  async function prepareObservedDirectConflict(dto: PageDto = page(strayPath, "the loaded disk baseline")) {
    loadSingle(dto);
    const loaded = pageByName(dto.name);
    expect(loaded).toBeDefined();
    setRaw(loaded!.roots[0], "the retained local draft");
    const savePage = vi.spyOn(backend(), "savePage")
      .mockRejectedValueOnce(new Error("conflict:41"));
    expect(await flushPage(dto.name)).toBe(false);
    expect(conflicts()).toEqual([dto.name]);
    return savePage;
  }

  function mountConflictWith(editor: () => ReturnType<typeof ConflictBar>) {
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <><ConflictBar />{editor()}</>, root);
    return { root, dispose };
  }

  it("adopts the pinned baseline without writing when the shown authority was withdrawn", async () => {
    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    const diskBaseline = page(strayPath, "the loaded disk baseline");
    const present = vi.spyOn(backend(), "presentConflictOverride")
      .mockResolvedValue("withdrawn");
    const getPageByPath = vi.spyOn(backend(), "getPageByPath")
      .mockResolvedValue(diskBaseline);
    const getPage = vi.spyOn(backend(), "getPage");
    // This is what the defective re-observing route would write. Leaving it
    // available makes the regression prove that no post-click save is issued,
    // rather than passing because an unexpected write happened to throw.
    savePage.mockResolvedValue("rev-of-retained-draft");
    const saveCallsBeforeClick = savePage.mock.calls.length;

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();

    await vi.waitFor(() => expect(conflicts()).toEqual([]));
    expect(present).toHaveBeenCalledWith(strayPath, diskBaseline.rev, expect.any(Number), 41);
    expect(getPageByPath).toHaveBeenCalledWith(strayPath);
    expect(getPage).not.toHaveBeenCalled();
    expect(savePage).toHaveBeenCalledTimes(saveCallsBeforeClick);
    expect(isDirty(sharedName)).toBe(false);
    const live = pageByName(sharedName);
    expect(live?.path).toBe(strayPath);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the loaded disk baseline");
    dispose();
  });

  it("re-observes instead of installing a divergent disk snapshot when authority was withdrawn", async () => {
    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    const divergent = page(strayPath, "a newer winner that was never authorised");
    vi.spyOn(backend(), "presentConflictOverride").mockResolvedValue("withdrawn");
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(divergent);
    savePage.mockRejectedValueOnce(new Error("conflict:42"));

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();

    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));
    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the retained local draft");
    expect(conflicts()).toContain(sharedName);
    dispose();
  });

  it("keeps post-click typing when replacement activation finishes later", async () => {
    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    vi.spyOn(backend(), "presentConflictOverride").mockResolvedValue("authorised");
    vi.spyOn(backend(), "getPageByPath")
      .mockResolvedValue(page(strayPath, "the authorised disk winner"));
    let releaseActivation: (handle: { activation: number; target: string; prospective: boolean }) => void = () => {};
    const activate = vi.spyOn(backend(), "activateEditor").mockReturnValue(
      new Promise((resolve) => {
        releaseActivation = resolve;
      }),
    );
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    savePage.mockRejectedValueOnce(new Error("conflict:42"));

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await vi.waitFor(() => expect(activate).toHaveBeenCalledTimes(1));
    const incumbent = pageByName(sharedName)!;
    setRaw(incumbent.roots[0], "typing after the discard click must survive");
    releaseActivation({ activation: 9002, target: strayPath, prospective: false });

    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));
    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe(
      "typing after the discard click must survive",
    );
    expect(conflicts()).toContain(sharedName);
    expect(retire).toHaveBeenCalledWith(strayPath, 9002);
    dispose();
  });

  it("keeps a title draft and its conflict when the rename lease starts during activation", async () => {
    const titleName = "Rename during discard";
    const titlePath = "pages/Rename during discard.md";
    const diskBaseline: PageDto = {
      name: titleName,
      kind: "page",
      title: titleName,
      pre_block: null,
      path: titlePath,
      rev: "title-baseline",
      blocks: [{
        id: "title-discard-block",
        raw: "the loaded disk baseline",
        collapsed: false,
        children: [],
        properties: [],
      }],
    };
    const savePage = await prepareObservedDirectConflict(diskBaseline);
    const getPageByPath = vi.spyOn(backend(), "getPageByPath").mockResolvedValue(diskBaseline);
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([]);
    vi.spyOn(backend(), "getUnlinkedRefs").mockResolvedValue([]);
    mainPaneRouter.openFile(titlePath, titleName, "page", { inPlace: true });
    const { root, dispose } = mountConflictWith(() => <PageView />);
    await vi.waitFor(() => expect(root.querySelector(".page-title")).not.toBeNull());

    let releaseActivation!: (value: { activation: number; target: string; prospective: boolean }) => void;
    const pendingActivation = new Promise<{ activation: number; target: string; prospective: boolean }>((resolve) => {
      releaseActivation = resolve;
    });
    vi.spyOn(backend(), "activateEditor").mockReturnValueOnce(pendingActivation);
    const present = vi.spyOn(backend(), "presentConflictOverride").mockResolvedValue("authorised");
    getPageByPath.mockResolvedValue({ ...diskBaseline, rev: "disk-winner", blocks: [
      { id: "disk-title-winner", raw: "the disk winner", collapsed: false, children: [], properties: [] },
    ] });
    savePage.mockRejectedValueOnce(new Error("conflict:42"));

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await vi.waitFor(() => expect(backend().activateEditor).toHaveBeenCalled());
    root.querySelector<HTMLElement>(".page-title")!.dispatchEvent(
      new MouseEvent("dblclick", { bubbles: true }),
    );
    const input = root.querySelector<HTMLInputElement>(".page-title-input")!;
    input.value = "post-click title draft";
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    releaseActivation({ activation: 9003, target: titlePath, prospective: false });

    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));
    expect(input.isConnected).toBe(true);
    expect(input.value).toBe("post-click title draft");
    expect(conflicts()).toContain(titleName);
    expect(shownObservationFor(titleName)).toBe(42);
    expect(present).not.toHaveBeenCalled();
    dispose();
  });

  it("keeps an active IME composition and restores an answerable conflict after presentation", async () => {
    const { dispose, savePage } = await mountWithObservedDirectConflict();
    const blockId = pageByName(sharedName)!.roots[0];
    dispose();
    startEditing(blockId, 0);
    const mounted = mountConflictWith(() => <Block id={blockId} />);
    const textarea = mounted.root.querySelector<HTMLTextAreaElement>("textarea.block-editor")!;
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(
      page(strayPath, "the authorised disk winner"),
    );
    vi.spyOn(backend(), "activateEditor").mockResolvedValue({
      activation: 9004,
      target: strayPath,
      prospective: false,
    });
    let releasePresentation!: (value: "authorised") => void;
    const presentation = new Promise<"authorised">((resolve) => {
      releasePresentation = resolve;
    });
    const present = vi.spyOn(backend(), "presentConflictOverride").mockReturnValue(presentation);
    savePage.mockRejectedValueOnce(new Error("conflict:42"));

    mounted.root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await vi.waitFor(() => expect(present).toHaveBeenCalledTimes(1));
    textarea.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    textarea.value = "post-click composing text";
    const composing = new InputEvent("input", {
      bubbles: true,
      inputType: "insertCompositionText",
      data: "字",
    });
    Object.defineProperty(composing, "isComposing", { value: true });
    textarea.dispatchEvent(composing);
    releasePresentation("authorised");

    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));
    expect(textarea.isConnected).toBe(true);
    expect(textarea.value).toBe("post-click composing text");
    expect(conflicts()).toContain(sharedName);
    expect(shownObservationFor(sharedName)).toBe(42);
    mounted.dispose();
  });

  it("keeps a compositionstart-absent IME transaction and restores an answerable conflict", async () => {
    const { dispose, savePage } = await mountWithObservedDirectConflict();
    const blockId = pageByName(sharedName)!.roots[0];
    dispose();
    startEditing(blockId, 0);
    const mounted = mountConflictWith(() => <Block id={blockId} />);
    const textarea = mounted.root.querySelector<HTMLTextAreaElement>("textarea.block-editor")!;
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(
      page(strayPath, "the authorised disk winner"),
    );
    vi.spyOn(backend(), "activateEditor").mockResolvedValue({
      activation: 9005,
      target: strayPath,
      prospective: false,
    });
    let releasePresentation!: (value: "authorised") => void;
    const presentation = new Promise<"authorised">((resolve) => {
      releasePresentation = resolve;
    });
    const present = vi.spyOn(backend(), "presentConflictOverride").mockReturnValue(presentation);
    savePage.mockRejectedValueOnce(new Error("conflict:42"));

    mounted.root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await vi.waitFor(() => expect(present).toHaveBeenCalledTimes(1));
    textarea.value = "post-click fallback composing text";
    const composing = new InputEvent("input", {
      bubbles: true,
      inputType: "insertCompositionText",
      data: "字",
    });
    Object.defineProperty(composing, "isComposing", { value: true });
    textarea.dispatchEvent(composing);
    expect(doc.byId[blockId].raw).toBe("the retained local draft");
    releasePresentation("authorised");

    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));
    expect(textarea.isConnected).toBe(true);
    expect(textarea.value).toBe("post-click fallback composing text");
    expect(doc.byId[blockId].raw).toBe("the retained local draft");
    expect(conflicts()).toContain(sharedName);
    expect(shownObservationFor(sharedName)).toBe(42);
    mounted.dispose();
  });

  it("installs managed current bytes without Direct presentation or activation", async () => {
    loadSingle(page(strayPath, "the managed baseline"));
    setRaw(pageByName(sharedName)!.roots[0], "the retained managed draft");
    const activate = vi.spyOn(backend(), "activateEditor").mockResolvedValue(null);
    const current = page(strayPath, "the current managed DTO");
    const getPageByPath = vi.spyOn(backend(), "getPageByPath").mockResolvedValue(current);
    const savePage = vi.spyOn(backend(), "savePage")
      .mockRejectedValueOnce(new Error("managed.conflict: stale_base"));
    expect(await flushPage(sharedName)).toBe(false);
    expect(conflicts()).toEqual([sharedName]);
    activate.mockClear();
    const present = vi.spyOn(backend(), "presentConflictOverride");

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <ConflictBar />, root);
    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();

    await vi.waitFor(() => expect(conflicts()).toEqual([]));
    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the current managed DTO");
    expect(savePage).toHaveBeenCalledTimes(1);
    expect(getPageByPath).toHaveBeenCalledTimes(2);
    expect(present).not.toHaveBeenCalled();
    expect(activate).not.toHaveBeenCalled();
    dispose();
  });

  it("keeps a post-click managed edit when the current DTO read finishes later", async () => {
    loadSingle(page(strayPath, "the managed baseline"));
    setRaw(pageByName(sharedName)!.roots[0], "the retained managed draft");
    const activate = vi.spyOn(backend(), "activateEditor").mockResolvedValue(null);
    const observed = page(strayPath, "the managed winner observed at conflict");
    let releaseRead!: (dto: PageDto) => void;
    const heldRead = new Promise<PageDto>((resolve) => {
      releaseRead = resolve;
    });
    const getPageByPath = vi.spyOn(backend(), "getPageByPath")
      .mockResolvedValueOnce(observed)
      .mockReturnValueOnce(heldRead);
    const savePage = vi.spyOn(backend(), "savePage")
      .mockRejectedValueOnce(new Error("managed.conflict: stale_base"));
    expect(await flushPage(sharedName)).toBe(false);
    expect(conflicts()).toEqual([sharedName]);
    activate.mockClear();
    const present = vi.spyOn(backend(), "presentConflictOverride");

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <ConflictBar />, root);
    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await vi.waitFor(() => expect(getPageByPath).toHaveBeenCalledTimes(2));

    const incumbent = pageByName(sharedName)!;
    setRaw(incumbent.roots[0], "the post-click managed edit");
    releaseRead(page(strayPath, "stale managed bytes from the click"));
    await heldRead;
    await Promise.resolve();

    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the post-click managed edit");
    expect(conflicts()).toEqual([sharedName]);
    expect(savePage).toHaveBeenCalledTimes(1);
    expect(present).not.toHaveBeenCalled();
    expect(activate).not.toHaveBeenCalled();
    dispose();
  });

  it("keeps a newer managed observation when an older click read finishes later", async () => {
    loadSingle(page(strayPath, "the managed baseline"));
    setRaw(pageByName(sharedName)!.roots[0], "the retained managed draft");
    const activate = vi.spyOn(backend(), "activateEditor").mockResolvedValue(null);
    const firstWinner = page(strayPath, "the first managed winner");
    const newerWinner = page(strayPath, "the newer managed winner");
    let releaseRead!: (dto: PageDto) => void;
    const heldRead = new Promise<PageDto>((resolve) => {
      releaseRead = resolve;
    });
    const getPageByPath = vi.spyOn(backend(), "getPageByPath")
      .mockResolvedValueOnce(firstWinner)
      .mockReturnValueOnce(heldRead)
      .mockResolvedValueOnce(newerWinner);
    const savePage = vi.spyOn(backend(), "savePage")
      .mockRejectedValueOnce(new Error("managed.conflict: stale_base"))
      .mockRejectedValueOnce(new Error("managed.conflict: stale_base"));
    expect(await flushPage(sharedName)).toBe(false);
    expect(conflicts()).toEqual([sharedName]);
    activate.mockClear();
    const present = vi.spyOn(backend(), "presentConflictOverride");

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <ConflictBar />, root);
    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await vi.waitFor(() => expect(getPageByPath).toHaveBeenCalledTimes(2));

    expect(await reobserve(sharedName)).toBe(false);
    expect(getPageByPath).toHaveBeenCalledTimes(3);
    expect(conflicts()).toEqual([sharedName]);
    expect(canForceSave(sharedName)).toBe(true);
    const newerObservation = managedConflictObservationSnapshotFor(sharedName);
    expect(newerObservation?.observation).toEqual({
      kind: "observed",
      path: strayPath,
      revision: newerWinner.rev,
    });
    // Re-observation itself goes through the ordinary save setup. Measure only
    // the older discard callback from the point its held actor read is released.
    activate.mockClear();
    releaseRead(page(strayPath, "stale managed bytes from the older click"));
    await heldRead;
    await Promise.resolve();

    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the retained managed draft");
    expect(conflicts()).toEqual([sharedName]);
    expect(canForceSave(sharedName)).toBe(true);
    expect(managedConflictObservationSnapshotFor(sharedName)).toEqual(newerObservation);
    expect(savePage).toHaveBeenCalledTimes(2);
    expect(present).not.toHaveBeenCalled();
    expect(activate).not.toHaveBeenCalled();
    dispose();
  });

  it("does not consume or save when the pre-consume disk read fails", async () => {
    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    const present = vi.spyOn(backend(), "presentConflictOverride").mockResolvedValue("authorised");
    vi.spyOn(backend(), "getPageByPath").mockRejectedValue(new Error("read failed"));
    const saveCallsBeforeClick = savePage.mock.calls.length;

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await Promise.resolve();
    await Promise.resolve();

    expect(present).not.toHaveBeenCalled();
    expect(savePage).toHaveBeenCalledTimes(saveCallsBeforeClick);
    expect(conflicts()).toContain(sharedName);
    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the retained local draft");
    dispose();
  });

  it("does not consume or save when replacement activation fails", async () => {
    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    const present = vi.spyOn(backend(), "presentConflictOverride").mockResolvedValue("authorised");
    vi.spyOn(backend(), "getPageByPath")
      .mockResolvedValue(page(strayPath, "the disk winner"));
    vi.spyOn(backend(), "activateEditor").mockRejectedValue(new Error("activation failed"));
    const saveCallsBeforeClick = savePage.mock.calls.length;

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    expect(present).not.toHaveBeenCalled();
    expect(savePage).toHaveBeenCalledTimes(saveCallsBeforeClick);
    expect(conflicts()).toContain(sharedName);
    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the retained local draft");
    dispose();
  });

  it("re-observes a superseded conflict instead of installing disk bytes", async () => {
    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    const present = vi.spyOn(backend(), "presentConflictOverride")
      .mockResolvedValue("superseded");
    const getPageByPath = vi.spyOn(backend(), "getPageByPath")
      .mockResolvedValue(page(strayPath, "newer disk bytes must not be installed"));
    savePage.mockRejectedValueOnce(new Error("conflict:42"));

    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();

    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));
    expect(present).toHaveBeenCalledWith(strayPath, expect.any(String), expect.any(Number), 41);
    expect(getPageByPath).toHaveBeenCalledWith(strayPath);
    const live = pageByName(sharedName);
    expect(live ? doc.byId[live.roots[0]]?.raw : undefined).toBe("the retained local draft");
    expect(conflicts()).toContain(sharedName);
    dispose();
  });

  it("refuses to install disk bytes read against a graph that has been reopened", async () => {
    // Acceptance row D2 requires the click to capture the graph binding and
    // re-check it at the final boundary. It captured the edit generation and the
    // instance generation but never the binding, so a backend reopen landing
    // between the click and the read — `changeJournalTitleFormat` and five other
    // settings all reach `refresh_graph`, which may MIGRATE journal filenames —
    // let bytes describing the old graph replace the user's unsaved work and
    // clear the banner. (GH #254 increment 3, round 15.)
    let landRead: (dto: unknown) => void = () => {};
    vi.spyOn(backend(), "getPageByPath").mockReturnValue(
      new Promise((r) => {
        landRead = r as (dto: unknown) => void;
      }) as never,
    );
    const { notifyGraphRebound } = await import("../modeHooks");

    const { root, dispose, savePage } = await mountWithObservedDirectConflict();
    savePage.mockRejectedValueOnce(new Error("conflict:42"));
    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();
    await Promise.resolve();

    // The graph is reopened while the disk read is outstanding...
    notifyGraphRebound();
    landRead(page(strayPath, "bytes from the graph that was replaced"));
    await Promise.resolve();
    await Promise.resolve();
    await vi.waitFor(() => expect(savePage).toHaveBeenCalledTimes(2));

    // ...so those bytes must not be installed over the editor's content, and the
    // banner must not be silently cleared as though the discard succeeded.
    // Read the actual node text. `FeedPage` has no `blocks` field — asserting on
    // one compares against "" and passes however the product behaves.
    const live = pageByName(sharedName);
    const text = live ? (doc.byId[live.roots[0]]?.raw ?? "") : "";
    expect(text, "stale bytes must not replace the editor's content").not.toContain(
      "was replaced",
    );
    expect(conflicts(), "nor may the banner be cleared as though it succeeded").toContain(
      sharedName,
    );
    dispose();
  });

  it("reloads the pinned file, not the canonical owner of the name", async () => {
    const getPageByPath = vi.spyOn(backend(), "getPageByPath")
      .mockImplementation(async (path) => (path === strayPath ? page(strayPath, "disk text of the stray") : null));
    const getPage = vi.spyOn(backend(), "getPage")
      .mockResolvedValue(page(canonicalPath, "the CANONICAL day, a different file"));

    const { root, dispose } = await mountWithObservedDirectConflict();
    const actions = root.querySelectorAll<HTMLButtonElement>(".conflict-btn");
    expect(actions[0].textContent?.trim()).toBe("Use current version");
    expect(actions[1].textContent?.trim()).toBe("Keep mine");
    expect(actions[1].disabled).toBe(false);
    actions[0].click();

    await vi.waitFor(() => expect(conflicts()).toEqual([]));
    expect(getPageByPath).toHaveBeenCalledWith(strayPath);
    expect(getPage).not.toHaveBeenCalled();
    expect(pageByName(sharedName)?.path).toBe(strayPath);
    dispose();
  });

  // The other half of the same pin: if the pinned file really is gone, falling
  // back to the name would resurrect the tab pointing at an unrelated file. The
  // page must be dropped, exactly as it is for an unpinned page.
  it("drops the page when the pinned file itself is gone", async () => {
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(null);
    const getPage = vi.spyOn(backend(), "getPage")
      .mockResolvedValue(page(canonicalPath, "the CANONICAL day, a different file"));

    const { root, dispose } = await mountWithObservedDirectConflict();
    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();

    await vi.waitFor(() => expect(pageByName(sharedName)).toBeUndefined());
    expect(getPage).not.toHaveBeenCalled();
    dispose();
  });

  // An ordinary page with no pin keeps resolving by name — a brand-new page has
  // no path at all, and name resolution is what finds its file once it exists.
  it("still resolves an unpinned page by name", async () => {
    const withoutPath: PageDto = { ...page(canonicalPath, "text"), path: undefined };
    loadSingle(withoutPath);
    const loaded = pageByName(sharedName)!;
    setRaw(loaded.roots[0], "the retained local draft");
    vi.spyOn(backend(), "savePage").mockRejectedValueOnce(new Error("conflict:41"));
    expect(await flushPage(sharedName)).toBe(false);
    const getPageByPath = vi.spyOn(backend(), "getPageByPath").mockResolvedValue(null);
    const getPage = vi.spyOn(backend(), "getPage")
      .mockResolvedValue(page(canonicalPath, "disk text"));

    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <ConflictBar />, root);
    root.querySelectorAll<HTMLButtonElement>(".conflict-btn")[0].click();

    await vi.waitFor(() => expect(conflicts()).toEqual([]));
    expect(getPage).toHaveBeenCalledWith(sharedName, "journal");
    expect(getPageByPath).not.toHaveBeenCalled();
    dispose();
  });
});
