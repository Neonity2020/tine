/** @jsxImportSource solid-js */
import { describe, expect, it, afterEach } from "vitest";
import { render } from "solid-js/web";
import { Portal } from "solid-js/web";
import { forbidsEditEntry } from "./editTargets";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

describe("forbidsEditEntry", () => {
  it("forbids entry for a press inside a PORTAL the host only owns logically", () => {
    // Solid delegates `mousedown` and walks `_$host`, so a press inside a
    // portal is delivered to the component's logical parent — for the query
    // sheet, that parent is the `.block-content.macro-host` whose mousedown
    // starts editing the block. The sheet floats over the page and is nowhere
    // inside that div; entering the editor would unmount the sheet under the
    // user's own click, before the click could ever reach the button.
    const seen: boolean[] = [];
    const host = document.createElement("div");
    document.body.append(host);
    dispose = render(
      () => (
        <div class="block-content macro-host" onMouseDown={(e) => seen.push(forbidsEditEntry(e))}>
          <span class="rendered-text">a block</span>
          <Portal>
            <div class="qs-sheet-anchor">
              <div class="qs-sheet">
                <button class="qb-sort" type="button">+ sort</button>
              </div>
            </div>
          </Portal>
        </div>
      ),
      host,
    );

    const button = document.querySelector<HTMLButtonElement>(".qb-sort")!;
    expect(host.contains(button), "the portal must be outside the host to be this test").toBe(false);
    button.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(seen, "the host never saw the portalled press at all").toEqual([true]);
  });

  it("still allows entry for a press on the block's own text", () => {
    const seen: boolean[] = [];
    const host = document.createElement("div");
    document.body.append(host);
    dispose = render(
      () => (
        <div class="block-content" onMouseDown={(e) => seen.push(forbidsEditEntry(e))}>
          <span class="rendered-text">a block</span>
        </div>
      ),
      host,
    );
    host.querySelector(".rendered-text")!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(seen).toEqual([false]);
  });

  it("still forbids entry for the block's own interactive children", () => {
    const seen: boolean[] = [];
    const host = document.createElement("div");
    document.body.append(host);
    dispose = render(
      () => (
        <div class="block-content" onMouseDown={(e) => seen.push(forbidsEditEntry(e))}>
          <button class="query-collapse" type="button">▾</button>
        </div>
      ),
      host,
    );
    host.querySelector(".query-collapse")!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(seen).toEqual([true]);
  });
});
