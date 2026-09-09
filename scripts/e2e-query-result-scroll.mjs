// Real task completion in a broad multiline query; retain geometry and DOM evidence.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";
import { enableManagedStorage } from "./lib/e2e-managed-activation.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";
await ensureDisplay();
const tmp = fs.mkdtempSync("/tmp/tine-query-result-scroll-");
const graph = path.join(tmp, "graph");
const hostPage = process.env.E2E_QUERY_HOST === "source" ? "Source 10" : "Dashboard";
const queryText = process.env.E2E_QUERY_TEXT || "(task TODO)";
const flatQuery = process.env.E2E_QUERY_FLAT === "1";
const managed = process.env.E2E_QUERY_MANAGED === "1";
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(path.join(graph, dir), { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(tmp, "xdg", dir), { recursive: true });
fs.writeFileSync(path.join(graph, "logseq/config.edn"), "{}\n");
if (hostPage === "Dashboard") fs.writeFileSync(path.join(graph, "pages/Dashboard.md"), `- {{query ${queryText}}}\n`);
const now = new Date();
const day = `${now.getFullYear()}_${String(now.getMonth()+1).padStart(2,"0")}_${String(now.getDate()).padStart(2,"0")}`;
fs.writeFileSync(path.join(graph, `journals/${day}.md`), "- Open [[Dashboard]]\n");
for (let p = 0; p < 20; p++) {
  const lines = [];
  for (let b = 0; b < 6; b++) {
    lines.push(`- TODO Task ${p}-${b}`);
    for (let line = 0; line < 5 + b; line++) lines.push(`  Multiline detail ${line} for task ${p}-${b}.`);
  }
  const name = `Source ${String(p).padStart(2,"0")}`;
  if (name === hostPage) lines.unshift(`- {{query ${queryText}}}`);
  fs.writeFileSync(path.join(graph, `pages/${name}.md`), lines.join("\n") + "\n");
}
const app = process.env.TINE_APP || `${process.env.HOME}/research/tine-query`;
const tdPath = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const port = Number(process.env.E2E_DRIVER_PORT || 4590);
const log = fs.openSync(path.join(tmp,"driver.log"),"w");
const wm = managed ? spawn(process.env.E2E_WINDOW_MANAGER || "openbox", ["--sm-disable"], {
  detached: true, stdio: ["ignore", log, log], env: process.env,
}) : undefined;
const td = spawn(tdPath, webdriverServerArgs(port,port+1,"/usr/bin/WebKitWebDriver"), {
  detached: true, stdio:["ignore",log,log], env:{...process.env,TINE_GRAPH:graph,
    XDG_DATA_HOME:path.join(tmp,"xdg/data"),XDG_CONFIG_HOME:path.join(tmp,"xdg/config"),XDG_CACHE_HOME:path.join(tmp,"xdg/cache"),
    WEBKIT_DISABLE_DMABUF_RENDERER:"1",WEBKIT_DISABLE_COMPOSITING_MODE:"1",LIBGL_ALWAYS_SOFTWARE:"1",GDK_BACKEND:"x11"},
});
console.log(JSON.stringify({artifact:tmp,app,managed}));
let browser;
try {
  await sleep(2500);
  browser = await remote({hostname:"127.0.0.1",port,path:"/",logLevel:"error",connectionRetryCount:1,connectionRetryTimeout:60000,
    capabilities:tauriCapabilities(app,"query-result-scroll")});
  await browser.$(".ls-block, .page-title").waitForExist({timeout:20000});
  if (managed) await enableManagedStorage(browser);
  await openPageByName(browser,hostPage);
  await browser.waitUntil(async () => Number(await browser.$(".query-count").getText())===120,{timeout:20000});
  // A real scroll activates intermediate lazy groups; jumping straight to the
  // target misses height changes from those previously mounted pages.
  if (process.env.E2E_QUERY_SCROLL === "incremental") {
    for (let step = 0; step < 100; step++) {
      const reached = await browser.execute(() => {
        const group = [...document.querySelectorAll(".query-group")].find(g=>g.querySelector(".query-page, .query-crumb")?.textContent?.trim()==="Source 10");
        const sc = group.closest(".main-content");
        if (group.getBoundingClientRect().top < sc.getBoundingClientRect().bottom) return true;
        sc.scrollTop += 400;
        return false;
      });
      if (reached) break;
      await sleep(180);
    }
  }
  await browser.execute((flat) => {
    const group = [...document.querySelectorAll(".query-group")].find(g=>g.querySelector(".query-page, .query-crumb")?.textContent?.trim()==="Source 10");
    if (!group) throw new Error("target query group absent");
    if (flat && !group.classList.contains("query-group-flat")) throw new Error("sorted flat presentation was not selected");
    group.scrollIntoView({block:"center"});
  }, flatQuery);
  await browser.waitUntil(async () => browser.execute(() => {
    const g=[...document.querySelectorAll(".query-group")].find(g=>g.querySelector(".query-page, .query-crumb")?.textContent?.trim()==="Source 10");
    return (g?.querySelectorAll(".block-task-checkbox").length??0)>=1;
  }),{timeout:15000});
  // A page sort may return a multi-block group. The renderer may also own its
  // rows individually; target the same task in either DOM structure.
  await browser.execute(() => {
    const rows=[...document.querySelectorAll(".query-group .ls-block")];
    if(rows.some(row=>row.textContent.includes("Task 10-1"))) return;
    const g=[...document.querySelectorAll(".query-group")].find(g=>g.querySelector(".query-page, .query-crumb")?.textContent?.trim()==="Source 10");
    g.nextElementSibling?.scrollIntoView({block:"center"});
  });
  await browser.waitUntil(async()=>browser.execute(()=>
    [...document.querySelectorAll(".query-group .ls-block")].some(row=>
      row.textContent.includes("Task 10-1") && row.querySelector(".block-task-checkbox"))),{timeout:15000});
  await browser.execute(() => {
    const row=[...document.querySelectorAll(".query-group .ls-block")].find(row=>row.textContent.includes("Task 10-1"));
    const target=row.querySelector(".block-task-checkbox");
    target.scrollIntoView({block:"center"});
    window.__queryScrollTarget=target;
  });
  await sleep(1500);
  const before=await browser.execute(() => {
    const target=window.__queryScrollTarget;
    const rows=[...document.querySelectorAll(".query-group .ls-block")];
    const targetIndex=rows.indexOf(target.closest(".ls-block"));
    if(targetIndex<1) throw new Error("target has no preceding result anchor");
    const anchor=rows[targetIndex-1];
    window.__queryScrollAnchor=anchor;
    window.__queryScrollGroup=anchor.closest(".query-group");
    window.__queryScrollScroller=target.closest(".main-content");
    const sc=window.__queryScrollScroller;
    return {scrollTop:sc.scrollTop,height:sc.scrollHeight,anchorTop:anchor.getBoundingClientRect().top,
      targetTop:target.getBoundingClientRect().top,targetId:target.closest(".ls-block").dataset.blockId,anchorId:anchor.dataset.blockId,count:document.querySelector(".query-count")?.textContent};
  });
  await browser.saveScreenshot(path.join(tmp,"before.png"));
  fs.writeFileSync(path.join(tmp,"before.json"),JSON.stringify(before,null,2));
  // The real control owns mousedown (to avoid entering the block editor).
  // HTMLElement.click() omits that event and does not toggle a task.
  await browser.$(`.query-group .ls-block[data-block-id="${before.targetId}"] .block-task-checkbox`).click();
  await browser.waitUntil(async()=>browser.execute((targetId) => {
    const row=[...document.querySelectorAll(".query-group .ls-block")].find(row=>row.dataset.blockId===targetId);
    return row===window.__queryScrollTarget?.closest(".ls-block")
      && row?.querySelector(".block-marker")?.textContent?.trim()==="DONE"
      && document.querySelector(".query-count")?.textContent?.trim()==="120";
  },before.targetId),{timeout:1500,timeoutMsg:"completed query row did not remain live during UI grace"});
  const duringGrace=await browser.execute(() => ({
    targetConnected:window.__queryScrollTarget?.isConnected??false,
    targetMarker:window.__queryScrollTarget?.closest(".ls-block")?.querySelector(".block-marker")?.textContent?.trim(),
    count:document.querySelector(".query-count")?.textContent,
  }));
  await browser.waitUntil(async()=>Number(await browser.$(".query-count").getText())===119,{timeout:15000});
  await sleep(1500);
  const after=await browser.execute(() => {
    const old=window.__queryScrollAnchor;
    const sc=window.__queryScrollScroller;
    const current=[...document.querySelectorAll(".query-group .ls-block")].find(n=>n.dataset.blockId===old.dataset.blockId);
    return {scrollTop:sc.scrollTop,height:sc.scrollHeight,anchorTop:current?.getBoundingClientRect().top,
      anchorConnected:old.isConnected,groupConnected:window.__queryScrollGroup.isConnected,
      count:document.querySelector(".query-count")?.textContent};
  });
  const report={app,managed,hostPage,queryText,flatQuery,before,duringGrace,after,anchorDrift:after.anchorTop-before.anchorTop};
  fs.writeFileSync(path.join(tmp,"report.json"),JSON.stringify(report,null,2));
  console.log(JSON.stringify(report));
  if(!after.anchorConnected || !after.groupConnected || Math.abs(report.anchorDrift)>24) throw new Error("Unchanged result anchor moved or remounted after task completion");
  // The assertions above, not image acquisition, are this geometry gate.
  // WebKit's second screenshot can stall after the dynamic row removal even
  // though script evaluation and repaint complete. Keep the exact measured
  // geometry and post-refresh DOM; before.png remains the visual baseline.
  fs.writeFileSync(path.join(tmp,"after.html"),await browser.execute(()=>document.body.outerHTML));
} catch(error) {
  try { await browser?.saveScreenshot(path.join(tmp,"failure.png")); fs.writeFileSync(path.join(tmp,"failure.html"),await browser.execute(()=>document.body.outerHTML)); } catch {}
  console.error(String(error)); process.exitCode=1;
} finally {
  try { await browser?.deleteSession(); } catch {}
  try { process.kill(-td.pid,"SIGKILL"); } catch {}
  try { if (wm?.pid) process.kill(-wm.pid, "SIGKILL"); } catch {}
  fs.closeSync(log);
}
