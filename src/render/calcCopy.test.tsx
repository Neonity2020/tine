import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { CalcBlock } from "./body";
import { initParser } from "./parse";
import { resetStore } from "../store";
import { clearClipboardPayload } from "../clipboard";
import { backend } from "../backend";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  clearClipboardPayload();
  resetStore();
  document.body.innerHTML = "";
});

function mount(src: string): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => <CalcBlock src={src} />, root);
  return { root, dispose };
}

describe("CalcBlock copy button (GH #228)", () => {
  it("offers a copy button only on successful result lines, not on errors or blanks", () => {
    const src = "1 + 1\n2 + 2\nbad /\n# comment\n:hex";
    const { root, dispose } = mount(src);

    const rows = [...root.querySelectorAll(".calc-out")];
    expect(rows).toHaveLength(5);

    const copyBtns = rows.map((r) => r.querySelector("button.copy-btn"));
    expect(copyBtns[0]).not.toBeNull(); // 1 + 1 → 2
    expect(copyBtns[1]).not.toBeNull(); // 2 + 2 → 4
    expect(copyBtns[2]).toBeNull();     // error
    expect(copyBtns[3]).toBeNull();     // comment (output null)
    expect(copyBtns[4]).toBeNull();     // :hex directive (output null)

    dispose();
  });

  it("copies the displayed result text on click via the clipboard facade", () => {
    const writeText = vi.spyOn(backend(), "writeText").mockResolvedValue();
    const { root, dispose } = mount("1 + 1\n2 + 2");

    const btns = root.querySelectorAll<HTMLButtonElement>(".calc-out button.copy-btn");
    expect(btns).toHaveLength(2);
    btns[1]!.click();

    expect(writeText).toHaveBeenCalledWith("4");

    dispose();
  });

  it("copy button click does not mutate the block or create an empty row", () => {
    const { root, dispose } = mount("3 + 4");
    vi.spyOn(backend(), "writeText").mockResolvedValue();
    const before = root.querySelectorAll(".calc-out").length;
    const btn = root.querySelector<HTMLButtonElement>(".calc-out button.copy-btn")!;
    btn.click();
    expect(root.querySelectorAll(".calc-out")).toHaveLength(before);
    dispose();
  });
});
