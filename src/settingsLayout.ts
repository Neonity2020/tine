// Whether the Settings dialog opens at its near-viewport size.
//
// GH #287 added the maximize control; GH #427 asked for it to be remembered:
// "I never want the small version. The whole point of #287 was to read settings
// at a comfortable size, and having to press the control on every open is a
// small tax paid several times a day."
//
// Deliberately module-level rather than a signal inside the dialog: Settings is
// lazy() and unmounts on close, so a component-local signal cannot survive a
// reopen, and hydrating one at mount would open the dialog small and then snap
// it wide. Persisted through the same device-local app_bool store the other
// remembered preferences use (tine-settings.json), so it also survives a
// restart. Default OFF — people who like the small dialog see no change.

import { createSignal } from "solid-js";
import { backend } from "./backend";

const KEY = "settings_dialog_maximized";

const [maximized, setMaximizedSig] = createSignal(false);

/** Reactive: the Settings dialog is at its near-viewport size. */
export const settingsMaximized = maximized;

export function setSettingsMaximized(on: boolean): void {
  setMaximizedSig(on);
  void backend().setAppBool(KEY, on).catch(() => {});
}

/** Load the remembered size at startup. Default: the small dialog. */
export async function initSettingsLayout(): Promise<void> {
  try {
    setMaximizedSig(await backend().getAppBool(KEY, false));
  } catch {
    /* default to the small dialog */
  }
}
