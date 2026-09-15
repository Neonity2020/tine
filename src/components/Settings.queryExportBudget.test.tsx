import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings } from "../ui";
import { backend } from "../backend";
import {
  DEFAULT_QUERY_EXPORT_BUDGET_MIB,
  initQueryExportBudget,
  queryExportBudgetBytes,
  queryExportBudgetMiB,
  resetQueryExportBudget,
} from "../queryExportBudget";

const KEY = "query_export_asset_budget_mib";
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  resetQueryExportBudget();
});

describe("Settings → Graph → Query export size limit", () => {
  it("changes the limit every export request carries, remembers it on this device, and resets", async () => {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);
    openSettings("graph");
    await tick();
    const input = root.querySelector<HTMLInputElement>('input[aria-label="Query export size limit in MiB"]')!;
    expect(Number(input.value)).toBe(DEFAULT_QUERY_EXPORT_BUDGET_MIB);

    input.value = "256";
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await tick();
    expect(queryExportBudgetMiB()).toBe(256);
    expect(queryExportBudgetBytes()).toBe(256 * 1024 * 1024);
    expect(await backend().getAppString(KEY, "")).toBe("256");

    // A restart reads the remembered value back.
    resetQueryExportBudget();
    await backend().setAppString(KEY, "256");
    await initQueryExportBudget();
    expect(queryExportBudgetMiB()).toBe(256);

    const reset = [...root.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Reset" && b.closest('[data-setting-label="Query export size limit"]'))!;
    reset.click();
    await tick();
    expect(queryExportBudgetMiB()).toBe(DEFAULT_QUERY_EXPORT_BUDGET_MIB);
    dispose();
  });
});
