import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import {
  doc,
  editorActivationFor,
  ensurePageLoaded,
  pageByName,
  resetStore,
} from "../store";
import type { EditorActivationHandle, PageDto } from "../types";
import { LiveRefGroup } from "./LiveRefGroup";

const dto = (path = "pages/Source.md", raw = "source block"): PageDto => ({
  name: "Source",
  kind: "page",
  title: "Source",
  pre_block: null,
  path,
  rev: `rev-${raw}`,
  blocks: [{ id: "source-block", raw, collapsed: false, children: [] }],
});

const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((yes) => {
    resolve = yes;
  });
  return { promise, resolve };
};

function mount() {
  const root = document.createElement("div");
  document.body.append(root);
  const page = dto();
  const dispose = render(() => (
    <LiveRefGroup
      page={page.name}
      kind={page.kind}
      path={page.path}
      blocks={page.blocks}
      surface="ref"
    />
  ), root);
  return { root, dispose };
}

function hierarchyDto(): PageDto {
  const page = dto();
  page.blocks = [{
    id: "source-block",
    raw: "source block",
    collapsed: false,
    children: [{ id: "source-child", raw: "source child", collapsed: false, children: [] }],
  }];
  return page;
}

afterEach(() => {
  resetStore();
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("LiveRefGroup editor activation", () => {
  it("activates a present editable DTO before installing it", async () => {
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(dto());
    const activation = deferred<EditorActivationHandle>();
    vi.spyOn(backend(), "activateEditor").mockReturnValue(activation.promise);
    const { dispose } = mount();

    await vi.waitFor(() => expect(backend().activateEditor).toHaveBeenCalledWith(
      "pages/Source.md",
      "replace",
      "rev-source block",
    ));
    expect(pageByName("Source")).toBeUndefined();

    activation.resolve({ activation: 90, target: "pages/Source.md", prospective: false });
    await vi.waitFor(() => expect(editorActivationFor("Source")).toBe(90));
    expect(doc.byId["source-block"]?.raw).toBe("source block");
    dispose();
  });

  it("compare-retires the minted activation when the group unmounts", async () => {
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(dto());
    const activation = deferred<EditorActivationHandle>();
    vi.spyOn(backend(), "activateEditor").mockReturnValue(activation.promise);
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    const { dispose } = mount();
    await vi.waitFor(() => expect(backend().activateEditor).toHaveBeenCalled());

    dispose();
    activation.resolve({ activation: 91, target: "pages/Source.md", prospective: false });
    await vi.waitFor(() => expect(retire).toHaveBeenCalledWith("pages/Source.md", 91));
    expect(pageByName("Source")).toBeUndefined();
  });

  it("does not replace a concurrent occupant that arrived during activation", async () => {
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(dto());
    const activation = deferred<EditorActivationHandle>();
    vi.spyOn(backend(), "activateEditor")
      .mockReturnValueOnce(activation.promise)
      .mockResolvedValueOnce({
        activation: 93,
        target: "pages/other/Source.md",
        prospective: false,
      });
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    const { dispose } = mount();
    await vi.waitFor(() => expect(backend().activateEditor).toHaveBeenCalledTimes(1));

    await ensurePageLoaded(dto("pages/other/Source.md", "concurrent occupant"));
    activation.resolve({ activation: 92, target: "pages/Source.md", prospective: false });

    await vi.waitFor(() => expect(retire).toHaveBeenCalledWith("pages/Source.md", 92));
    expect(pageByName("Source")?.path).toBe("pages/other/Source.md");
    expect(doc.byId[pageByName("Source")!.roots[0]]?.raw).toBe("concurrent occupant");
    expect(editorActivationFor("Source")).toBe(93);
    dispose();
  });

  it("hydrates every sibling group when concurrent activation of their exact source has one winner", async () => {
    const page = hierarchyDto();
    vi.spyOn(backend(), "getPageByPath").mockResolvedValue(page);
    const first = deferred<EditorActivationHandle>();
    const second = deferred<EditorActivationHandle>();
    vi.spyOn(backend(), "activateEditor")
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    const retire = vi.spyOn(backend(), "retireEditorActivation").mockResolvedValue(true);
    const root = document.createElement("div");
    document.body.append(root);
    const shallow = [{ ...page.blocks[0], children: [] }];
    const dispose = render(() => (
      <>
        <LiveRefGroup page={page.name} kind={page.kind} path={page.path} blocks={shallow} surface="embed" />
        <LiveRefGroup page={page.name} kind={page.kind} path={page.path} blocks={shallow} surface="embed" />
      </>
    ), root);

    await vi.waitFor(() => expect(backend().activateEditor).toHaveBeenCalledTimes(2));
    first.resolve({ activation: 94, target: page.path!, prospective: false });
    await vi.waitFor(() => expect(doc.byId["source-child"]?.raw).toBe("source child"));
    second.resolve({ activation: 95, target: page.path!, prospective: false });

    await vi.waitFor(() => expect(root.textContent?.match(/source child/g)).toHaveLength(2));
    expect(retire).toHaveBeenCalledWith(page.path, 95);
    expect(editorActivationFor(page.name)).toBe(94);
    dispose();
  });
});
