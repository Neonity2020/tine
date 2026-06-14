// Minimal in-app routing: either the journals feed or a single named page.
import { createSignal } from "solid-js";

export type Route =
  | { kind: "journals" }
  | { kind: "page"; name: string; pageKind: "journal" | "page" };

export const [route, setRoute] = createSignal<Route>({ kind: "journals" });

export function openPage(name: string, pageKind: "journal" | "page" = "page") {
  setRoute({ kind: "page", name, pageKind });
}

export function openJournals() {
  setRoute({ kind: "journals" });
}
