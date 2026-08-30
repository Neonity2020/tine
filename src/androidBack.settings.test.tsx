import { afterAll, afterEach, describe, expect, it, vi } from "vitest";

// `isMobilePlatform` is a module-load constant read from the WebView UA, and
// the drawer classifier is a media query. Both have to be the Android values
// BEFORE the app modules are imported, so this file loads them dynamically.
const userAgentDescriptor = Object.getOwnPropertyDescriptor(navigator, "userAgent");
const matchMediaDescriptor = Object.getOwnPropertyDescriptor(globalThis, "matchMedia");
Object.defineProperty(navigator, "userAgent", {
  configurable: true,
  value: "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Mobile Safari/537.36",
});
const narrow = {
  matches: true,
  media: "(max-width: 639px)",
  onchange: null,
  addEventListener() {},
  removeEventListener() {},
  addListener() {},
  removeListener() {},
  dispatchEvent() { return true; },
} as unknown as MediaQueryList;
Object.defineProperty(globalThis, "matchMedia", { configurable: true, value: () => narrow });

const { render } = await import("solid-js/web");
const { App } = await import("./App");
const { dispatchAndroidBack } = await import("./androidBack");
const { dismissTopTransient } = await import("./transientLayers");
const { restoreDrawerFocus } = await import("./mobileDrawers");
const { isMobilePlatform } = await import("./nativeChrome");
const { closeSettings, dismissMobileDrawer, openSettings, settingsOpen } = await import("./ui");

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const waitForSettingsModal = (host: HTMLElement) => vi.waitFor(
  () => expect(host.querySelector(".settings-modal")).not.toBeNull(),
  { timeout: 5_000 },
);

let dispose = () => {};

/** The rungs exactly as App.tsx wires them for the native SafeBack listener.
 * `history` and `root` record instead of acting: a Back that reached either of
 * them with the Settings modal on screen is the production defect, where the
 * route behind the modal moves and the modal itself never closes. */
function pressBack() {
  const fallbacks: string[] = [];
  const disposition = dispatchAndroidBack(
    { canGoBack: true },
    {
      dismissTransient: () => dismissTopTransient("back"),
      dismissDrawer: () => dismissMobileDrawer("back"),
      restoreDrawerFocus: () => restoreDrawerFocus("back"),
      // Records instead of navigating, and reports that it moved: a Back that
      // reached this rung with the Settings modal on screen is the defect.
      historyBack: () => { fallbacks.push("history"); return true; },
      closeRoot: () => { fallbacks.push("root"); },
    },
  );
  return { disposition, fallbacks };
}

async function mountApp() {
  const host = document.createElement("div");
  document.body.append(host);
  const disposeApp = render(() => <App />, host);
  dispose = () => {
    disposeApp();
    dispose = () => {};
  };
  // The whole production composition has to be live, not a Settings-only
  // fixture: the interesting failure was always another layer winning the rung.
  await vi.waitFor(() => expect(host.querySelector("header.topbar")).not.toBeNull());
  return host;
}

afterEach(async () => {
  dispose();
  closeSettings();
  await tick();
  document.body.innerHTML = "";
});

afterAll(() => {
  if (userAgentDescriptor) Object.defineProperty(navigator, "userAgent", userAgentDescriptor);
  if (matchMediaDescriptor) Object.defineProperty(globalThis, "matchMedia", matchMediaDescriptor);
  else delete (globalThis as { matchMedia?: typeof matchMedia }).matchMedia;
});

describe("Android Back and the Settings modal", () => {
  it("mounts the Android/narrow composition rather than a desktop one", async () => {
    await mountApp();
    expect(isMobilePlatform).toBe(true);
  });

  it("peels recording, then the search query, then closes — three presses, never the history rung", async () => {
    const host = await mountApp();
    openSettings("shortcuts");
    await waitForSettingsModal(host);

    const keycap = () => [...host.querySelectorAll<HTMLElement>(".help-shortcut-row")]
      .find((row) => row.querySelector(".help-shortcut-id")?.textContent === "go/find-in-page")
      ?.querySelector<HTMLButtonElement>(".help-keycap-button");
    expect(keycap()).toBeDefined();

    // Rung 1: an in-progress shortcut recording.
    keycap()!.click();
    await tick();
    expect(keycap()!.textContent).toBe("Press keys...");
    const recordingPress = pressBack();
    await tick();
    expect(recordingPress).toEqual({ disposition: "transient", fallbacks: [] });
    expect(keycap()!.textContent).not.toBe("Press keys...");
    expect(settingsOpen()).toBe(true);

    // Rung 2: a non-empty settings search query.
    const search = host.querySelector<HTMLInputElement>(".settings-search-input")!;
    search.value = "Find in page";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    await tick();
    expect(host.querySelector(".settings-search-results")).toBeNull();
    expect([...host.querySelectorAll<HTMLElement>(".help-shortcut-id")].map((id) => id.textContent))
      .toEqual(["go/find-in-page"]);
    const queryPress = pressBack();
    await tick();
    expect(queryPress).toEqual({ disposition: "transient", fallbacks: [] });
    expect(host.querySelectorAll(".help-shortcut-id").length).toBeGreaterThan(1);
    expect(settingsOpen()).toBe(true);

    // Rung 3: the modal itself.
    const closePress = pressBack();
    await tick();
    expect(closePress).toEqual({ disposition: "transient", fallbacks: [] });
    expect(settingsOpen()).toBe(false);
    expect(host.querySelector(".settings-modal")).toBeNull();
  });

  // Martin, Aug 18, on the APK that first made Tine the topmost Back owner:
  // "back for nav is broken - but it wasn't, some time today". Before that
  // change Tauri's AppPlugin answered Back with webView.goBack(), so ordinary
  // navigation worked and only modals were unreachable. Now every gesture is
  // ours, and a plain page with nothing open must therefore reach the history
  // rung — anything above it silently eating the gesture is the new defect.
  it("reaches the history rung on a plain page with nothing open", async () => {
    await mountApp();
    const { activeDrawer, setLeftSidebarOpen } = await import("./ui");
    // A fresh mount opens the left drawer, and closing it is a legitimate rung.
    // The question here is what happens once nothing is open at all.
    setLeftSidebarOpen(false);
    await tick();
    expect(activeDrawer()).toBeNull();

    const trace: string[] = [];
    for (let i = 0; i < 3; i += 1) {
      const press = pressBack();
      await tick();
      trace.push(press.disposition);
    }
    expect(trace).toEqual(["history", "history", "history"]);
  });

  it("closes the modal even while a mobile drawer is open underneath it", async () => {
    const { setLeftSidebarOpen, activeDrawer } = await import("./ui");
    const host = await mountApp();
    setLeftSidebarOpen(true);
    await tick();
    expect(activeDrawer()).toBe("left");

    openSettings();
    await waitForSettingsModal(host);

    const press = pressBack();
    await tick();
    // The drawer is a lower rung by construction; one gesture must not take
    // both, and it must take the modal first.
    expect(press).toEqual({ disposition: "transient", fallbacks: [] });
    expect(settingsOpen()).toBe(false);
    expect(activeDrawer()).toBe("left");
  });
});
