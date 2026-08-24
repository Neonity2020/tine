// Graph home page (GH #245/#269): an optional page that opens automatically in
// the primary tab. Logseq-compatible config.edn owns the value; the old
// device-local string is read only for one-time migration.
import { backend } from "./backend";
import { openPage } from "./router";
import { graphMeta, setGraphMeta } from "./ui";

const KEY_PREFIX = "home.page.";

export async function getHomePageSetting(root: string): Promise<string> {
  const meta = graphMeta();
  const configured = meta?.root === root ? (meta.default_home ?? "").trim() : "";
  if (configured) return configured;

  try {
    const legacy = (await backend().getAppString(KEY_PREFIX + root, "")).trim();
    if (!legacy) return "";
    try {
      await backend().setDefaultHome(legacy);
      const current = graphMeta();
      if (current?.root === root) setGraphMeta({ ...current, default_home: legacy });
      // The graph-owned commit is authoritative. Clearing the obsolete local
      // cache is best-effort and must not make a successful graph write appear
      // to have failed.
      await backend().setAppString(KEY_PREFIX + root, "").catch(() => {});
    } catch {
      // A malformed graph-owned value is refused by the native writer. Keep
      // honoring the legacy value for this session and retry on a later read;
      // never replace graph bytes we do not understand.
    }
    return legacy;
  } catch {
    return "";
  }
}

export async function setHomePageSetting(root: string, name: string | null): Promise<boolean> {
  const value = (name ?? "").trim();
  try {
    await backend().setDefaultHome(value || null);
    const meta = graphMeta();
    if (meta?.root === root) setGraphMeta({ ...meta, default_home: value || null });
    await backend().setAppString(KEY_PREFIX + root, "").catch(() => {});
    return true;
  } catch {
    return false;
  }
}

/** Navigate the primary tab to the graph's configured home page. Falling back
 *  to the ordinary landing is silent: nothing is created, no toast, no retry —
 *  a deleted/renamed page just means the normal landing stays (GH #245).
 *  Resolves true when it actually navigated, so callers (e.g. the `gh`
 *  hotstring) can fall through to their own landing otherwise. */
export async function openConfiguredHomePage(
  root: string,
  isCurrent: () => boolean = () => true,
): Promise<boolean> {
  const name = (await getHomePageSetting(root)).trim();
  if (!name || !isCurrent()) return false;
  const dto = await backend().getPage(name, "page").catch(() => null);
  if (!dto || !isCurrent()) return false;
  openPage(dto.name, "page", { inPlace: true });
  return true;
}
