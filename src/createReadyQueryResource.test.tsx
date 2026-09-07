import { createSignal, For } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { QueryNotReadyError } from "./backend";
import { createReadyQueryResource } from "./createReadyQueryResource";
import { setGraphMeta } from "./ui";
import { notifyGraphRebound } from "./modeHooks";
import { graphBinding } from "./persistence";
import { resetSharedQueryResultsForTests, sharedQueryResult, sharedQueryScope } from "./queryResultCache";

afterEach(() => { resetSharedQueryResultsForTests(); setGraphMeta(null); document.body.innerHTML = ""; vi.restoreAllMocks(); });

it("clears the old graph's rows while the new graph is pending", async () => {
  setGraphMeta({ root: "first" } as never);
  const load = vi.fn<() => Promise<string[]>>().mockResolvedValueOnce(["old graph"])
    .mockRejectedValue(new QueryNotReadyError("indexing"));
  const root = document.createElement("div");
  const dispose = render(() => {
    const [rows] = createReadyQueryResource(() => "same query", load);
    return <span>{rows()?.join(",")}</span>;
  }, root);
  try {
    await vi.waitFor(() => expect(root.textContent).toBe("old graph"));
    setGraphMeta({ root: "second" } as never);
    await vi.waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    expect(root.textContent).not.toContain("old graph");
  } finally { dispose(); }
});

it("restarts on a same-root binding replacement", async () => {
  const load = vi.fn<() => Promise<string[]>>().mockResolvedValueOnce(["old binding"])
    .mockResolvedValue(["new binding"]);
  const root = document.createElement("div");
  const dispose = render(() => {
    const [rows] = createReadyQueryResource(() => "same query", key =>
      sharedQueryResult(sharedQueryScope("root", 0, graphBinding()), key, load));
    return <span>{rows()?.join(",")}</span>;
  }, root);
  try {
    await vi.waitFor(() => expect(root.textContent).toBe("old binding"));
    notifyGraphRebound();
    await vi.waitFor(() => expect(root.textContent).toBe("new binding"));
  } finally { dispose(); }
});

it("retries indexing automatically while retaining existing row DOM", async () => {
  const [key, setKey] = createSignal(1);
  const load = vi.fn<(key: number) => Promise<string[]>>().mockResolvedValueOnce(["one", "two"])
    .mockRejectedValueOnce(new QueryNotReadyError("indexing"))
    .mockResolvedValue(["one"]);
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = render(() => {
    const [rows, pending] = createReadyQueryResource(key, load);
    return <><span role="status">{pending()?.message}</span><For each={rows()}>{id => <div data-row={id}>{id}</div>}</For></>;
  }, root);
  try {
    await vi.waitFor(() => expect(root.querySelectorAll("[data-row]").length).toBe(2));
    const first = root.querySelector('[data-row="one"]');
    setKey(2);
    await vi.waitFor(() => expect(root.querySelector('[role="status"]')?.textContent).toContain("Updating"));
    expect(root.querySelector('[data-row="one"]')).toBe(first);
    expect(root.querySelectorAll("[data-row]").length).toBe(2);
    await vi.waitFor(() => expect(root.querySelectorAll("[data-row]").length).toBe(1));
    expect(root.querySelector('[data-row="one"]')).toBe(first);
    expect(load).toHaveBeenCalledTimes(3);
  } finally { dispose(); }
});

it("stops pending retries when the query is disabled or unmounted", async () => {
  const [key, setKey] = createSignal<string | undefined>("query");
  const load = vi.fn().mockRejectedValue(new QueryNotReadyError("busy"));
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = render(() => {
    const [, pending] = createReadyQueryResource(key, load);
    return <span>{pending()?.message}</span>;
  }, root);
  try {
    await vi.waitFor(() => expect(root.textContent).toContain("Updating"));
    setKey(undefined);
    const count = load.mock.calls.length;
    await new Promise(resolve => setTimeout(resolve, 220));
    expect(load).toHaveBeenCalledTimes(count);
    setKey("again");
    await vi.waitFor(() => expect(load.mock.calls.length).toBeGreaterThan(count));
  } finally { dispose(); }
  const count = load.mock.calls.length;
  await new Promise(resolve => setTimeout(resolve, 220));
  expect(load).toHaveBeenCalledTimes(count);
});

it("drops an obsolete answer even after the source returns to its original value", async () => {
  const [key, setKey] = createSignal("A");
  let finish!: (value: string[]) => void;
  const load = vi.fn<(key: string) => Promise<string[]>>().mockImplementationOnce(() => new Promise<string[]>(resolve => { finish = resolve; }))
    .mockResolvedValue(["current"]);
  const root = document.createElement("div");
  const dispose = render(() => {
    const [rows] = createReadyQueryResource(key, load);
    return <span>{rows()?.join(",")}</span>;
  }, root);
  try {
    await vi.waitFor(() => expect(load).toHaveBeenCalledTimes(1));
    setKey("B");
    setKey("A");
    await vi.waitFor(() => expect(root.textContent).toBe("current"));
    finish(["obsolete"]);
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(root.textContent).toBe("current");
  } finally { dispose(); }
});
