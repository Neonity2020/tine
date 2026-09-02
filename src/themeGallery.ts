import { createSignal } from "solid-js";
import { backend } from "./backend";
import { CUSTOM_CSS_STYLE_ID, LS_SHIM_STYLE_ID, ensureLsShimStyle } from "./lsShim";
import { serializedWrites } from "./serializedWrites";
import { galleryThemeById, galleryThemes } from "./styles/themes";
import { installedThemeByKey } from "./themes/manager";
import type { ThemePresentation } from "./themes/manifest";

export const THEME_GALLERY_STYLE_ID = "tine-theme";
const LEGACY_KEY = "theme.gallery";
const COMPOSITION_KEY = "theme.composition.v1";

const [selectedStyleId, setSelectedStyleId] = createSignal("");
const [selectedColorId, setSelectedColorId] = createSignal("");
const [selectedPresentation, setSelectedPresentation] = createSignal<ThemePresentation>({});
const compositionWrites = serializedWrites("theme.composition.v1");

export const selectedThemeStyle = selectedStyleId;
export const selectedThemeColors = selectedColorId;
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

function themeById(id: string) {
  return id ? galleryThemeById(id) ?? installedThemeByKey(id) : undefined;
}

function presentationId(id: string): string {
  const theme = themeById(id);
  return theme && "manifest" in theme && Object.keys(theme.manifest.presentation ?? {}).length > 0
    ? theme.id
    : "";
}

function colorId(id: string): string {
  return themeById(id)?.id ?? "";
}

function persistComposition(): void {
  void compositionWrites.run(() => backend().setAppString(COMPOSITION_KEY, JSON.stringify({
    // Read inside the shared-key tail so a queued write cannot retain an older
    // composition after a newer selection has already been published.
    style: selectedStyleId(),
    colors: selectedColorId(),
  }))).catch(() => {});
}

function applyComposition(style: string, colors: string): void {
  const nextStyle = presentationId(style);
  const nextColors = colorId(colors);
  const styleTheme = themeById(nextStyle);
  const colorTheme = themeById(nextColors);
  setSelectedStyleId(nextStyle);
  setSelectedColorId(nextColors);
  applyPresentation(styleTheme && "manifest" in styleTheme ? styleTheme.manifest.presentation ?? {} : {});
  const el = ensureThemeStyle();
  if (el) el.textContent = colorTheme?.css ?? "";
  persistComposition();
}

/** Compatibility preset: select one theme's presentation and colors together. */
export function applyTheme(id: string): void {
  applyComposition(id, id);
}

export function applyThemeStyle(id: string): void {
  applyComposition(id, selectedColorId());
}

export function applyThemeColors(id: string): void {
  applyComposition(selectedStyleId(), id);
}

export function clearThemeSelection(id: string): void {
  applyComposition(
    selectedStyleId() === id ? "" : selectedStyleId(),
    selectedColorId() === id ? "" : selectedColorId(),
  );
}

export function reapplyThemeSelection(): void {
  applyComposition(selectedStyleId(), selectedColorId());
}

export async function initThemeGallery(): Promise<void> {
  ensureThemeStyle();
  let composition = "";
  try {
    composition = await backend().getAppString(COMPOSITION_KEY, "");
  } catch {
    composition = "";
  }
  if (composition) {
    try {
      const value: unknown = JSON.parse(composition);
      if (value && typeof value === "object" && !Array.isArray(value)) {
        const { style, colors } = value as Record<string, unknown>;
        if (typeof style === "string" && typeof colors === "string") {
          applyComposition(style, colors);
          return;
        }
      }
    } catch {}
  }

  let legacyId = "";
  try {
    legacyId = await backend().getAppString(LEGACY_KEY, "");
  } catch {
    legacyId = "";
  }
  applyTheme(legacyId);
}

if (typeof window !== "undefined") {
  window.__tineApplyTheme = applyTheme;
}
