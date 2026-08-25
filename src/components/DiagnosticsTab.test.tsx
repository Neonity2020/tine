import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "../backend";
import { DiagnosticsTab } from "./DiagnosticsTab";

describe("DiagnosticsTab", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    document.body.innerHTML = "";
  });

  it("creates a previewable privacy-safe current-and-previous-run report", async () => {
    vi.spyOn(backend(), "diagnosticReport").mockResolvedValue({
      text: "{\n  \"schemaVersion\": 1,\n  \"sessions\": [\"current\", \"previous\"]\n}",
      suggestedFileName: "tine-diagnostics-123.json",
    });
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <DiagnosticsTab />, root);

    const create = [...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Create diagnostic report")
    ) as HTMLButtonElement;
    create.click();

    await vi.waitFor(() => expect(root.querySelector("textarea")?.value).toContain('"schemaVersion": 1'));
    expect(root.textContent).toContain("Nothing is uploaded automatically");
    expect(root.textContent).toContain("page titles");
    dispose();
  });

  it("copies only the generated report and can clear retained events", async () => {
    vi.spyOn(backend(), "diagnosticReport").mockResolvedValue({
      text: "safe-report",
      suggestedFileName: "tine-diagnostics.json",
    });
    const copy = vi.spyOn(backend(), "writeText").mockResolvedValue();
    const clear = vi.spyOn(backend(), "clearDiagnostics").mockResolvedValue();
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <DiagnosticsTab />, root);

    ([...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Create diagnostic report")
    ) as HTMLButtonElement).click();
    await vi.waitFor(() => expect(root.querySelector("textarea")?.value).toBe("safe-report"));

    ([...root.querySelectorAll("button")].find((button) => button.textContent === "Copy report") as HTMLButtonElement).click();
    await vi.waitFor(() => expect(copy).toHaveBeenCalledWith("safe-report"));

    ([...root.querySelectorAll("button")].find((button) => button.textContent === "Clear recorded events") as HTMLButtonElement).click();
    await vi.waitFor(() => expect(clear).toHaveBeenCalled());
    expect(root.querySelector("textarea")).toBeNull();
    dispose();
  });
});
