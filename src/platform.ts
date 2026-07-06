import { backend } from "./backend";

let platformPromise: Promise<"android" | "ios" | "desktop"> | null = null;

export async function platformKind(): Promise<"android" | "ios" | "desktop"> {
  if (!platformPromise) platformPromise = backend().appPlatform();
  return platformPromise;
}

export async function isMobile(): Promise<boolean> {
  const kind = await platformKind();
  return kind === "android" || kind === "ios";
}
