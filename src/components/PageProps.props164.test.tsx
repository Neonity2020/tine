// GH #164: editing ARBITRARY properties from the UI, not just the five presets.
//
// Acceptance rows A1 and A3 of the packet contract. The panel today renders
// exactly `PAGE_PROP_SPECS`, so a property that is genuinely present in the
// file — `status:: draft` — is invisible and uneditable, and there is no way to
// add a new key at all. Both tests below therefore fail before the feature and
// name the user-visible harm rather than the implementation.
//
// Kept in its own file on purpose: transientFallbacks.p1d2.test.tsx reaches for
// `host.querySelector(".pp-input")` — the FIRST one — so the preset fields must
// keep their position, and that file's assumptions must not be perturbed here.

import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { type JSX } from "solid-js";
import { render } from "solid-js/web";
import { PageProps } from "./PageProps";
import { closePageProps, openBlockProps, openPageProps } from "../ui";
import { clearTransientLayersForTest } from "../transientLayers";
import { blockProperty, doc, loadSingle, readPageProperty, resetStore } from "../store";
import { initParser } from "../render/parse";
import { clearSeededFacets } from "../render/facets";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function mount(node: () => JSX.Element) {
  const host = document.createElement("div");
  document.body.append(host);
  return { host, dispose: render(node, host) };
}

function loadPage(name: string, preBlock: string | null) {
  loadSingle({
    name,
    kind: "page",
    title: name,
    pre_block: preBlock,
    blocks: [{ id: "body-1", raw: "body", collapsed: false, children: [] }],
  });
}

function fieldLabels(host: HTMLElement): string[] {
  return Array.from(host.querySelectorAll<HTMLElement>(".pp-field .pp-label"))
    .map((el) => el.textContent ?? "");
}

function typeInto(input: HTMLInputElement, value: string) {
  input.focus();
  input.value = value;
  input.dispatchEvent(new InputEvent("input", { bubbles: true, cancelable: true }));
}

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  closePageProps();
  clearTransientLayersForTest();
  resetStore();
  document.body.innerHTML = "";
});

describe("page properties panel — arbitrary keys (GH #164)", () => {
  it("lists a property the page actually has, even when it is not a preset", async () => {
    loadPage("Notes", "status:: draft");
    openPageProps("Notes", 20, 20);
    const { host, dispose } = mount(() => <PageProps />);
    try {
      await tick();
      // Fail-before: the panel renders only PAGE_PROP_SPECS, so `status` is
      // present in the file, shown in the under-title list, and yet has no field
      // here — the user can see the property but cannot change it.
      expect(fieldLabels(host)).toContain("status");
    } finally {
      dispose();
    }
  });

  it("adds a new property from the panel, and the canonical reader reads it back", async () => {
    loadPage("Notes", "status:: draft");
    openPageProps("Notes", 20, 20);
    const { host, dispose } = mount(() => <PageProps />);
    try {
      await tick();
      // Fail-before: there is no add-row at all, so this query returns null.
      const keyInput = host.querySelector<HTMLInputElement>("input.pp-add-key");
      const valueInput = host.querySelector<HTMLInputElement>("input.pp-add-value");
      expect(keyInput).not.toBeNull();
      expect(valueInput).not.toBeNull();

      typeInto(keyInput!, "owner");
      typeInto(valueInput!, "martin");
      host.querySelector<HTMLButtonElement>("button.pp-add-commit")!.click();
      await tick();

      // The outcome is the file's content read through the canonical reader,
      // not the panel's own state.
      expect(readPageProperty("Notes", "owner")).toBe("martin");
      // And the property that was already there is untouched.
      expect(readPageProperty("Notes", "status")).toBe("draft");
    } finally {
      dispose();
    }
  });

  // A3. The same panel, block scope. Fail-before for this row is RECONSTRUCTED,
  // not proven: at 0a1d537 `blockActions` carried no Properties item and
  // openBlockProps did not exist, so the absence is plain by inspection, but a
  // true red is no longer obtainable on this tree.
  it("lists and edits a block's own properties in block scope", async () => {
    loadSingle({
      name: "Notes",
      kind: "page",
      title: "Notes",
      pre_block: null,
      blocks: [{ id: "body-1", raw: "task\nowner:: martin", collapsed: false, children: [] }],
    });
    // A test DTO carries no parsed `properties`, so the DTO-seeded facet cache
    // would hold EMPTY facets for this block and mask the derive-from-raw path;
    // the real backend always ships them. Same reason and same remedy as the
    // `load()` helper in src/store.test.ts:122-130.
    clearSeededFacets();

    // Guard the layers below the UI first. If the block never landed under this
    // id, or its properties are not readable from the store, then a failing UI
    // assertion below would be blamed on the panel while the real fault is the
    // harness — the vacuous-proof shape this packet has been avoiding throughout.
    expect(doc.byId["body-1"]?.raw).toBe("task\nowner:: martin");
    expect(blockProperty("body-1", "owner")).toBe("martin");

    openBlockProps("body-1", 20, 20);
    const { host, dispose } = mount(() => <PageProps />);
    try {
      await tick();
      // The block's own property is listed, and the PAGE presets are not: they
      // are page-level keys and would write to the wrong subject entirely.
      expect(fieldLabels(host)).toContain("owner");
      expect(fieldLabels(host)).not.toContain("Aliases");

      const input = Array.from(host.querySelectorAll<HTMLInputElement>("input.pp-input"))
        .find((el) => !el.classList.contains("pp-add-key") && !el.classList.contains("pp-add-value"));
      expect(input).not.toBeUndefined();
      typeInto(input!, "martina");
      input!.dispatchEvent(new FocusEvent("blur", { bubbles: true }));
      await tick();

      expect(blockProperty("body-1", "owner")).toBe("martina");
    } finally {
      dispose();
    }
  });

  // A4. A page Tine cannot structurally round-trip is shown but never rewritten,
  // so it must offer no property editing at all — not a form whose writes are
  // silently dropped.
  it("offers no property editing on a read-only page", async () => {
    loadSingle({
      name: "Readonly",
      kind: "page",
      title: "Readonly",
      pre_block: "status:: draft",
      blocks: [{ id: "ro-1", raw: "body", collapsed: false, children: [] }],
      read_only: true,
    });
    openPageProps("Readonly", 20, 20);
    const { host, dispose } = mount(() => <PageProps />);
    try {
      await tick();
      // The panel still opens — the user asked for it, and it explains itself —
      // but nothing in it can write.
      expect(host.querySelector(".page-props-panel")).not.toBeNull();
      expect(host.querySelectorAll("input.pp-input").length).toBe(0);
      expect(host.querySelector("button.pp-add-commit")).toBeNull();
    } finally {
      dispose();
    }
  });
});
