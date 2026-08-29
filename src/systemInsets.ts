import { isTauri } from "./backend";

export type SystemInsetOwner = "native-viewport" | "css-viewport";

export function systemInsetOwner(nativeHost: boolean, userAgent: string): SystemInsetOwner {
  return nativeHost && /Android/i.test(userAgent) ? "native-viewport" : "css-viewport";
}

/**
 * Name the one layer that excludes system bars before any application UI is
 * rendered. MainActivity already sizes Android's WebView inside native insets;
 * every other surface keeps CSS env(safe-area-inset-*) ownership.
 */
export function installSystemInsetOwner(
  root: HTMLElement = document.documentElement,
  nativeHost: boolean = isTauri(),
  userAgent: string = navigator.userAgent,
): SystemInsetOwner {
  const owner = systemInsetOwner(nativeHost, userAgent);
  root.dataset.systemInsets = owner;
  return owner;
}
