import { createSignal } from "solid-js";
import { backend } from "./backend";
import { CUSTOM_CSS_STYLE_ID, LS_SHIM_STYLE_ID, ensureLsShimStyle } from "./lsShim";
import { galleryThemeById, galleryThemes } from "./styles/themes";
import { installedThemeByKey } from "./themes/manager";
import type { ThemePresentation } from "./themes/manifest";

export const THEME_GALLERY_STYLE_ID = "tine-theme";
const KEY = "theme.gallery";

const [selectedId, setSelectedId] = createSignal("");
const [selectedPresentation, setSelectedPresentation] = createSignal<ThemePresentation>({});

export const selectedGalleryTheme = selectedId;
export const selectedThemePresentation = selectedPresentation;
export { galleryThemes };

const PRESENTATION_ATTRIBUTES = {
  contentTypography: "data-theme-content-typography",
  journalHeader: "data-theme-journal-header",
  todayTaskSummary: "data-theme-today-task-summary",
} as const;

function applyPresentation(presentation: ThemePresentation): void {
  setSelectedPresentation(presentation);
  if (typeof document === "undefined") return;
  for (const [key, attribute] of Object.entries(PRESENTATION_ATTRIBUTES)) {
    const value = presentation[key as keyof ThemePresentation];
    if (value === undefined || value === "default" || value === "hidden") {
      document.documentElement.removeAttribute(attribute);
    } else {
      document.documentElement.setAttribute(attribute, value);
    }
  }
}

export function ensureThemeStyle(): HTMLStyleElement | null {
  if (typeof document === "undefined") return null;

  const shim = ensureLsShimStyle();
  let el = document.getElementById(THEME_GALLERY_STYLE_ID) as HTMLStyleElement | null;
  if (el && el.tagName !== "STYLE") {
    el.remove();
    el = null;
  }
  if (!el) {
    el = document.createElement("style");
    el.id = THEME_GALLERY_STYLE_ID;
  }

  const custom = document.getElementById(CUSTOM_CSS_STYLE_ID);
  const afterShim = shim?.parentNode === document.head ? shim.nextSibling : null;
  if (shim?.parentNode === document.head && afterShim !== el) {
    document.head.insertBefore(el, afterShim);
  } else if (custom?.parentNode === document.head) {
    document.head.insertBefore(el, custom);
  } else if (el.parentNode !== document.head) {
    document.head.appendChild(el);
  }

  const shimAgain = document.getElementById(LS_SHIM_STYLE_ID);
  const customAgain = document.getElementById(CUSTOM_CSS_STYLE_ID);
  if (shimAgain?.parentNode === document.head && shimAgain.nextSibling !== el) {
    document.head.insertBefore(el, shimAgain.nextSibling);
  }
  if (customAgain?.parentNode === document.head) {
    const nodes = Array.from(document.head.childNodes);
    if (nodes.indexOf(el) > nodes.indexOf(customAgain)) {
      document.head.insertBefore(el, customAgain);
    }
  }

  return el;
}

export function applyTheme(id: string): void {
  const theme = id ? galleryThemeById(id) ?? installedThemeByKey(id) : undefined;
  const nextId = theme?.id ?? "";
  setSelectedId(nextId);
  applyPresentation(theme && "manifest" in theme ? theme.manifest.presentation ?? {} : {});
  const el = ensureThemeStyle();
  if (el) el.textContent = theme?.css ?? "";
  void backend().setAppString(KEY, nextId).catch(() => {});
}

export async function initThemeGallery(): Promise<void> {
  ensureThemeStyle();
  let id = "";
  try {
    id = await backend().getAppString(KEY, "");
  } catch {
    id = "";
  }
  applyTheme(id);
}

if (typeof window !== "undefined") {
  window.__tineApplyTheme = applyTheme;
}
