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

  it("compares graph bytes and identifies the exact differing source path", async () => {
    const local = {
      schemaVersion: 1,
      tool: "tine-graph-bytes",
      algorithm: "sha256",
      complete: true,
      generatedAtUnixMs: 1,
      files: [{ path: "journals/2026_06_19.md", length: 85, digest: "a".repeat(64) }],
      aggregateDigest: "b".repeat(64),
      errors: [],
    };
    const other = {
      ...local,
      files: [{ path: "journals/2026_06_19.md", length: 85, digest: "c".repeat(64) }],
      aggregateDigest: "d".repeat(64),
    };
    vi.spyOn(backend(), "createGraphVerification").mockResolvedValue({
      text: JSON.stringify(local),
      suggestedFileName: "tine-graph-verification.json",
      totalFiles: 1,
      totalBytes: 85,
      aggregateDigest: local.aggregateDigest,
      complete: true,
    });
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <DiagnosticsTab />, root);

    ([...root.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Create graph verification")
    ) as HTMLButtonElement).click();
    await vi.waitFor(() => expect(root.textContent).toContain("1 files"));
    const textareas = root.querySelectorAll("textarea");
    const otherInput = textareas[textareas.length - 1];
    otherInput.value = JSON.stringify(other);
    otherInput.dispatchEvent(new InputEvent("input", { bubbles: true }));
    ([...root.querySelectorAll("button")].find((button) =>
      button.textContent === "Compare reports"
    ) as HTMLButtonElement).click();

    await vi.waitFor(() => expect(root.textContent).toContain("Different bytes"));
    expect(root.textContent).toContain("journals/2026_06_19.md");
    expect(root.textContent).toContain("file paths and page names");
    dispose();
  });
});
