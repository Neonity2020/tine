import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { createStartupRecoveryController, STARTUP_LOOKUP_WATCHDOG_MS } from "../startupRecovery";
import { StartupRecoveryLayer } from "./StartupRecovery";

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = "";
});

describe("startup recovery surface", () => {
  it("offers all cold-start escapes without displaying or copying the raw graph path", async () => {
    vi.useFakeTimers();
    const copyText = vi.fn(async (_text: string) => {});
    const controller = createStartupRecoveryController({
      lookupGraphPath: () => new Promise(() => {}),
      injectedGraphPath: () => "",
      persistedGraphPath: () => "/home/martin/private/Research graph",
      openGraph: vi.fn(),
      pickGraph: vi.fn(),
      coldReturn: vi.fn(),
      acceptColdReturn: vi.fn(),
      confirmColdReturn: vi.fn(async () => false),
      copyText,
      notify: vi.fn(),
      completeFirstLoad: vi.fn(),
    });
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => <StartupRecoveryLayer controller={controller} />, host);

    controller.start();
    await vi.advanceTimersByTimeAsync(STARTUP_LOOKUP_WATCHDOG_MS);
    expect(host.querySelector("[role=alertdialog]")).not.toBeNull();
    expect(host.textContent).toContain("Retry lookup");
    expect(host.textContent).toContain("Open another graph");
    expect(host.textContent).toContain("Return Research graph to Direct Files");
    expect(host.textContent).toContain("Copy details");
    expect(host.textContent).not.toContain("/home/martin/private");

    const copy = [...host.querySelectorAll("button")]
      .find((button) => button.textContent?.includes("Copy details"));
    copy?.click();
    await Promise.resolve();
    expect(copyText).toHaveBeenCalledOnce();
    expect(copyText.mock.calls[0][0]).not.toContain("/home/martin/private");

    controller.dispose();
    dispose();
  });
});
