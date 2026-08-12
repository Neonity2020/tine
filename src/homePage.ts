// Graph home page (GH #245): an optional per-graph page that opens
// automatically in the primary tab when the graph is opened. Stored per graph
// in the ordinary app-settings owner (a string keyed by graph root); no
// separate persistence and no page creation — a configured title that no
// longer resolves simply falls back to the ordinary landing.
import { backend } from "./backend";
import { openPage } from "./router";

const KEY_PREFIX = "home.page.";

export async function getHomePageSetting(root: string): Promise<string> {
  try {
    return (await backend().getAppString(KEY_PREFIX + root, "")).trim();
  } catch {
    return "";
  }
}

export async function setHomePageSetting(root: string, name: string | null): Promise<void> {
  try {
    await backend().setAppString(KEY_PREFIX + root, (name ?? "").trim());
  } catch {
    // Best-effort; the settings row re-reads the value when it opens.
  }
}

/** Navigate the primary tab to the graph's configured home page. Falling back
 *  to the ordinary landing is silent: nothing is created, no toast, no retry —
 *  a deleted/renamed page just means the normal landing stays (GH #245). */
export async function openConfiguredHomePage(
  root: string,
  isCurrent: () => boolean = () => true,
): Promise<void> {
  const name = (await getHomePageSetting(root)).trim();
  if (!name || !isCurrent()) return;
  const dto = await backend().getPage(name, "page").catch(() => null);
  if (!dto || !isCurrent()) return;
  openPage(dto.name, "page", { inPlace: true });
}
