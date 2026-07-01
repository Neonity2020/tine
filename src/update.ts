// "A newer Tine is available" check — best-effort, once per launch.
//
// Tine doesn't auto-update (the full Tauri updater needs signing keys + a
// per-platform install story, including macOS notarization — deferred). This is
// the cheap, high-value half: ask GitHub for the latest *published* release and,
// if it's newer than the running build, show a sticky toast with a one-click
// "Download" that opens the releases page in the system browser.
//
// Deliberately quiet: Tauri-only (the browser mock has no real version and no
// business phoning home), and silent on ANY failure (offline, rate-limited,
// blocked) — it must never block startup or nag with an error.

import { isTauri, backend } from "./backend";
import { pushToast } from "./ui";

const REPO = "martinkoutecky/tine";
const RELEASES_PAGE = `https://github.com/${REPO}/releases/latest`;
const LATEST_API = `https://api.github.com/repos/${REPO}/releases/latest`;

/** Parse the first `X.Y.Z` out of a version/tag string (`v0.3.0`, `0.3.0`, …). */
function parseVer(s: string): [number, number, number] | null {
  const m = /(\d+)\.(\d+)\.(\d+)/.exec(s);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

/** Is `a` a strictly newer semver triple than `b`? */
function isNewer(a: [number, number, number], b: [number, number, number]): boolean {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i];
  }
  return false;
}

/** Check GitHub for a newer published release; toast if there is one. Resolves
 *  silently (never throws) in every failure case. */
export async function checkForUpdate(): Promise<void> {
  if (!isTauri()) return;
  try {
    const { getVersion } = await import("@tauri-apps/api/app");
    const cur = parseVer(await getVersion());
    if (!cur) return;

    // `/releases/latest` is the newest NON-prerelease, NON-draft release.
    const res = await fetch(LATEST_API, {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!res.ok) return;
    const data: unknown = await res.json();
    const tag = (data as { tag_name?: unknown })?.tag_name;
    const latest = typeof tag === "string" ? parseVer(tag) : null;
    if (!latest || !isNewer(latest, cur)) return;

    pushToast(
      `Tine ${latest.join(".")} is available — you're on ${cur.join(".")}.`,
      "info",
      {
        sticky: true,
        action: {
          label: "Download",
          run: () => void backend().openExternal(RELEASES_PAGE).catch(() => {}),
        },
      }
    );
  } catch {
    // offline / rate-limited / network blocked — never bother the user.
  }
}
