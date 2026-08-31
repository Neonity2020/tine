// Window-chrome preferences (device-local, persisted in tine-settings.json via the
// generic app_bool backend so native startup can read them before a WebView exists).
//
// Tine's main window is frameless by default (`decorations: false`) so the toolbar
// doubles as the title bar and we save a row — see WindowChrome.tsx. That custom
// chrome reads as alien on macOS (square corners, no traffic lights — GitHub #3), so:
//
//   - macOS: a build-time override (tauri.macos.conf.json) gives the main window
//     `titleBarStyle: "Overlay"` + `hiddenTitle` → native rounded corners + traffic
//     lights, with the content still rising under the transparent title bar (the
//     compact layout is kept). The OS draws the controls, so our custom
//     WindowControls/ResizeGrips are never shown on macOS. Nothing here toggles it;
//     `isMac` just tells the UI to hide custom chrome + reserve the traffic-light gap.
//
//   - Linux/Windows: a restart-time toggle (default OFF = the custom frameless
//     chrome). Tao cannot reliably change GTK decorations on an existing window,
//     so Rust applies the preference while constructing every graph window.
//
// The capture mini-window is deliberately frameless and ignores this preference.

import { createSignal } from "solid-js";
import { backend } from "./backend";

export const KEY_NATIVE_FRAME = "native_window_frame";

declare global {
  // Set by Tauri before frontend code runs. Unlike the saved preference, this
  // describes the decorations actually applied to this process's windows.
  var __TINE_NATIVE_FRAME__: boolean | undefined;
  // Set by Tauri before frontend code runs (src-tauri/src/lib.rs), from the
  // build's own `app_platform()`. See platformKind below.
  var __TINE_PLATFORM__: "android" | "ios" | "desktop" | undefined;
}

export type PlatformKind = "android" | "ios" | "desktop";

// The user agent cannot answer "what am I running on". iPadOS 13+ serves a
// desktop-class `Macintosh; Intel Mac OS X` UA from a stock WKWebView — wry
// sets no `preferredContentMode` override, and adding one would only relocate
// the lie — so an iPad both FAILED the mobile test and PASSED the Mac test, and
// rendered as a Mac desktop throughout (GH #446: no editing toolbar on iPad).
//
// So the build tells us instead. Rust injects `__TINE_PLATFORM__` before any
// frontend code runs, which keeps this synchronous: the backend `app_platform`
// command says the same thing (see platform.ts) but a one-frame async
// round-trip would flash desktop-only chrome.
//
// The UA branch below is the fallback for contexts Tauri never initializes —
// `npm run dev` in a plain browser, the mock backend, and unit tests.
function detectPlatformKind(): PlatformKind {
  const injected = typeof globalThis !== "undefined" ? globalThis.__TINE_PLATFORM__ : undefined;
  if (injected === "android" || injected === "ios" || injected === "desktop") return injected;
  const ua = typeof navigator !== "undefined" ? (navigator.userAgent ?? "") : "";
  if (/Android/i.test(ua)) return "android";
  if (/iPhone|iPad|iPod/i.test(ua)) return "ios";
  return "desktop";
}

/** What the app is actually running on, decided by the build, not the UA. */
export const platformKind: PlatformKind = detectPlatformKind();

/** Android or iOS — a mobile OS that owns the window and drives touch input. */
export const isMobilePlatform: boolean = platformKind !== "desktop";

// macOS detection: WKWebView's UA contains "Macintosh"/"Mac OS X". navigator.platform
// is deprecated but a reliable fallback. Evaluated once — and only ever consulted
// on desktop, because iPadOS reports both of those strings too.
export const isMac: boolean =
  platformKind === "desktop" &&
  typeof navigator !== "undefined" &&
  (/Mac/i.test(navigator.platform ?? "") || /Mac OS X|Macintosh/i.test(navigator.userAgent ?? ""));

/** Tablet-or-larger: the shorter viewport edge has room for two documents
 *  side by side. iPad mini portrait is 744pt across, every iPhone in either
 *  orientation is at most 430pt. */
export function isTabletViewport(
  width = typeof window === "undefined" ? 0 : window.innerWidth,
  height = typeof window === "undefined" ? 0 : window.innerHeight,
): boolean {
  return Math.min(width, height) >= 700;
}

/** Does this device present the single-pane mobile shell?
 *
 *  Split panes are not a desktop-OS privilege — they need screen room and a
 *  precise pointer, both of which a tablet has. Gating them on `isMobilePlatform`
 *  alone would have taken panes away from iPad the moment #446 made that flag
 *  tell the truth. Phones keep the single-pane shell in either orientation. */
export function isSinglePaneShell(): boolean {
  return isMobilePlatform && !isTabletViewport();
}

// Linux/Windows user preference: use the OS-native window frame instead of our
// custom frameless chrome.
const startupNativeFrame = typeof globalThis !== "undefined" && globalThis.__TINE_NATIVE_FRAME__ === true;
const [nativeFrameActive] = createSignal(startupNativeFrame);
const [nativeFramePreference, setNativeFramePreferenceSig] = createSignal(startupNativeFrame);

/** Reactive: is the OS drawing the window controls (so our custom chrome should
 *  hide)? True on macOS always (the Overlay title bar provides traffic lights),
 *  on Linux/Windows when the user has turned the native frame on, and always on
 *  mobile (Android/iOS have no in-app min/max/close — the OS owns the window). */
export const osDrawsWindowControls = (): boolean =>
  isMac || nativeFrameActive() || isMobilePlatform;

/** Reactive state of the Linux/Windows native-frame toggle (for the Settings switch).
 *  Meaningless on macOS (where the native frame is always on). */
export const nativeFrameEnabled = nativeFramePreference;

/** Persist the Linux/Windows native-frame preference. It takes effect at the next
 *  normal app start, when Rust can construct all graph windows consistently. */
export async function setNativeFrame(on: boolean): Promise<void> {
  if (isMac) return;
  await backend().setAppBool(KEY_NATIVE_FRAME, on);
  setNativeFramePreferenceSig(on);
}

/** Read the saved preference for the Settings switch. Rust already applied the
 *  startup value before constructing this window. */
export async function initNativeChrome(): Promise<void> {
  if (isMac) return; // Overlay frame is fixed in tauri.macos.conf.json
  let on = startupNativeFrame;
  try {
    on = await backend().getAppBool(KEY_NATIVE_FRAME, startupNativeFrame);
  } catch {
    on = startupNativeFrame;
  }
  setNativeFramePreferenceSig(on);
}
