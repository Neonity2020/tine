import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

function rustModuleSource(root) {
  const files = [root];
  const visit = (directory) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const child = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(child);
      else if (entry.isFile() && entry.name.endsWith(".rs")) files.push(child);
    }
  };
  const moduleDirectory = root.replace(/\.rs$/, "");
  if (fs.existsSync(moduleDirectory)) visit(moduleDirectory);
  return files.sort().map((file) => fs.readFileSync(file, "utf8")).join("\n");
}

const source = rustModuleSource(
  fileURLToPath(new URL("../src-tauri/src/commands.rs", import.meta.url)),
);
const start = source.indexOf("pub(crate) fn tine_open_devtools");
const end = source.indexOf("\n#[tauri::command]", start + 1);
if (start < 0 || end < 0) throw new Error("could not locate tine_open_devtools");
const body = source.slice(start, end);

const attach = body.indexOf("connect_attach");
const open = body.lastIndexOf("window.open_devtools()");
if (attach < 0) throw new Error("Linux devtools must subscribe to WebKit's attach signal");
if (open < 0 || attach > open) throw new Error("the attach hook must be installed before devtools open");
const waylandGuard = body.indexOf("display().backend().is_wayland()");
if (waylandGuard < 0 || waylandGuard > attach) {
  throw new Error("native Wayland must stay docked before the X11/XWayland detach hook is armed");
}
if (!body.includes("idle_add_local_once")) {
  throw new Error("detach must run after WebKit finishes the attach signal, without a timer");
}
if (!body.includes("disconnect(handler_id)")) {
  throw new Error("the automatic detach hook must disconnect after its first attach event");
}
if (/sleep\s*\(|timeout_add|setTimeout/.test(body)) {
  throw new Error("devtools detach must not depend on a timeout");
}

console.log("Developer-tools detach lifecycle OK: Wayland-safe, hook-before-open, one-shot, timer-free.");
