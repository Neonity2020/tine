import type { EventCallback, UnlistenFn } from "@tauri-apps/api/event";

/** Listen for an event the backend addresses to THIS window (`emit_to(label, …)`).
 *
 * A bare `listen` registers with target `Any`, and Tauri hands an `Any`
 * listener every window's copy of a targeted event: window A's
 * `graph-rebound` rebound window B, aborting B's exports and prints, and A's
 * `warm-cache-done` ended B's wait early (GH #543, audit R10-05). Registering
 * with this window's own target is the one way to hear only its own events.
 * A broadcast (`app.emit`) still arrives; such events carry an identity in
 * their payload the listener checks, and are listed in
 * `windowEvents.test.ts`. */
export async function listenHere<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  const [{ listen }, { getCurrentWebviewWindow }] = await Promise.all([
    import("@tauri-apps/api/event"),
    import("@tauri-apps/api/webviewWindow"),
  ]);
  return listen<T>(event, handler, {
    target: { kind: "WebviewWindow", label: getCurrentWebviewWindow().label },
  });
}
