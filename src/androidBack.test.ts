import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";

function rustModuleSource(path: string): string {
  const files = [path];
  const visit = (directory: string) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const child = join(directory, entry.name);
      if (entry.isDirectory()) visit(child);
      else if (entry.isFile() && entry.name.endsWith(".rs")) files.push(child);
    }
  };
  const moduleDirectory = path.replace(/\.rs$/, "");
  if (existsSync(moduleDirectory)) visit(moduleDirectory);
  return files.sort().map((file) => readFileSync(file, "utf8")).join("\n");
}
import {
  dispatchAndroidBack,
  installAndroidBackHandler,
  type AndroidBackDispatchDeps,
  type AndroidBackListener,
} from "./androidBack";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

function dispatchDeps(): AndroidBackDispatchDeps & {
  transient: boolean;
  drawer: boolean;
  movedBack: boolean;
} {
  const state = {
    transient: false,
    drawer: false,
    movedBack: true,
    dismissTransient: vi.fn(() => state.transient),
    dismissDrawer: vi.fn(() => state.drawer),
    restoreDrawerFocus: vi.fn(),
    historyBack: vi.fn(() => state.movedBack),
    closeRoot: vi.fn(),
  };
  return state;
}

describe("GH #161 Android SafeBack owner", () => {
  it("peels exactly transient, drawer, one history step, then root close", () => {
    const deps = dispatchDeps();
    deps.transient = true;
    expect(dispatchAndroidBack({ canGoBack: true }, deps)).toBe("transient");
    expect(deps.dismissDrawer).not.toHaveBeenCalled();
    expect(deps.historyBack).not.toHaveBeenCalled();

    deps.transient = false;
    deps.drawer = true;
    expect(dispatchAndroidBack({ canGoBack: true }, deps)).toBe("drawer");
    expect(deps.restoreDrawerFocus).toHaveBeenCalledOnce();
    expect(deps.historyBack).not.toHaveBeenCalled();

    deps.drawer = false;
    expect(dispatchAndroidBack({ canGoBack: true }, deps)).toBe("history");
    expect(deps.historyBack).toHaveBeenCalledOnce();
    expect(deps.closeRoot).not.toHaveBeenCalled();

    // Root is reached by the router having nothing left to pop, which is the
    // only thing that distinguishes it from the rung above.
    deps.movedBack = false;
    expect(dispatchAndroidBack({ canGoBack: false }, deps)).toBe("root");
    expect(deps.historyBack).toHaveBeenCalledTimes(2);
    expect(deps.closeRoot).toHaveBeenCalledOnce();
  });

  it("subscribes exactly once only on Android and unregisters idempotently", async () => {
    const deps = dispatchDeps();
    const unregister = vi.fn(async () => {});
    let handler: ((payload: { canGoBack: boolean }) => void) | undefined;
    const subscribe = vi.fn(async (next) => {
      handler = next;
      return { unregister };
    });
    const uninstall = installAndroidBackHandler({
      ...deps,
      platform: async () => "android",
      subscribe,
    });
    await vi.waitFor(() => expect(subscribe).toHaveBeenCalledOnce());
    expect(handler).toBeTypeOf("function");
    handler!({ canGoBack: true });
    expect(deps.historyBack).toHaveBeenCalledOnce();

    uninstall();
    uninstall();
    expect(unregister).toHaveBeenCalledOnce();
  });

  it.each(["desktop", "ios"] as const)("does not subscribe on %s", async (platform) => {
    const deps = dispatchDeps();
    const subscribe = vi.fn();
    installAndroidBackHandler({ ...deps, platform: async () => platform, subscribe });
    await Promise.resolve();
    await Promise.resolve();
    expect(subscribe).not.toHaveBeenCalled();
  });

  it("takes the router's answer, not the WebView's, for the history rung", () => {
    // A phone reported canGoBack=true with nothing for the router to pop, so
    // Back landed on the history rung and silently did nothing. The rung is
    // chosen by whether the router actually moved.
    const deps = dispatchDeps();
    deps.movedBack = false;
    expect(dispatchAndroidBack({ canGoBack: true }, deps)).toBe("root");
    expect(deps.historyBack).toHaveBeenCalledOnce();
    expect(deps.closeRoot).toHaveBeenCalledOnce();

    deps.movedBack = true;
    expect(dispatchAndroidBack({ canGoBack: false }, deps)).toBe("history");
    expect(deps.closeRoot).toHaveBeenCalledOnce();
  });

  it("leaves the native SafeBack owner blocking when platform or subscription setup rejects", async () => {
    for (const failure of ["platform", "subscribe"] as const) {
      const deps = dispatchDeps();
      const setupFailed = vi.fn();
      const subscribe = vi.fn(async () => {
        if (failure === "subscribe") throw new Error("subscription failed");
        return { unregister: vi.fn() };
      });
      installAndroidBackHandler({
        ...deps,
        platform: async () => {
          if (failure === "platform") throw new Error("platform failed");
          return "android";
        },
        subscribe,
        setupFailed,
      });
      await vi.waitFor(() => expect(setupFailed).toHaveBeenCalledOnce());
      expect(deps.historyBack).not.toHaveBeenCalled();
      expect(deps.closeRoot).not.toHaveBeenCalled();
    }
  });

  it("does not subscribe when cleanup wins the pending platform race", async () => {
    const platform = deferred<"android">();
    const deps = dispatchDeps();
    const subscribe = vi.fn();
    const uninstall = installAndroidBackHandler({ ...deps, platform: () => platform.promise, subscribe });
    uninstall();
    platform.resolve("android");
    await Promise.resolve();
    await Promise.resolve();
    expect(subscribe).not.toHaveBeenCalled();
  });

  it("unregisters immediately when cleanup wins the pending subscription race", async () => {
    const installed = deferred<AndroidBackListener>();
    const deps = dispatchDeps();
    const subscribe = vi.fn(() => installed.promise);
    const unregister = vi.fn(async () => {});
    const uninstall = installAndroidBackHandler({
      ...deps,
      platform: async () => "android",
      subscribe,
    });
    await vi.waitFor(() => expect(subscribe).toHaveBeenCalledOnce());
    uninstall();
    installed.resolve({ unregister });
    await vi.waitFor(() => expect(unregister).toHaveBeenCalledOnce());
  });

  it("uses a permanent SafeBack owner and never revives the AppPlugin fallback", () => {
    const activity = readFileSync(
      "src-tauri/gen/android/app/src/main/java/page/tine/app/MainActivity.kt",
      "utf8",
    );
    const safeBackPlugin = readFileSync(
      "src-tauri/gen/android/app/src/main/java/page/tine/app/SafeBackPlugin.kt",
      "utf8",
    );
    const app = readFileSync("src/App.tsx", "utf8");

    expect(activity).not.toContain("handleBackNavigation");
    expect(activity).toContain("OnBackPressedCallback(true)");
    expect(activity).toContain("SafeBackBridge.dispatchIfReady()");
    expect(activity).toContain("showBlockedBackNotice()");
    expect(activity).toContain("SafeBackBridge.clear()");
    expect(activity).not.toContain("onBackPressedDispatcher.onBackPressed");
    expect(app).not.toContain('@tauri-apps/api/app');
    expect(app).toContain('addPluginListener("safe-back", "android-safe-back", handler)');
    expect(app).not.toContain("onBackButtonPress");
    expect(app).not.toContain('addEventListener("popstate"');
    expect(app).toContain("createAndroidRootCloseCoordinator(safeClose, {");
    expect(app).toContain('safeClose.prepare()) !== "accepted"');
    expect(app).toMatch(/catch \{\s*\/\/ The native close attempt failed[\s\S]*?allowClose = false;[\s\S]*?safeClose\.reset\(\);[\s\S]*?closeInProgress = false;/);
    expect(safeBackPlugin).toContain("private var webView: WebView? = null");
    expect(safeBackPlugin).toContain('hasListener("android-safe-back")');
    expect(safeBackPlugin).toContain('trigger("android-safe-back"');
    expect(safeBackPlugin).toContain("fun dispatchIfReady(): Boolean");
    // An inlined plugin gets no ACL manifest unless the build script declares
    // one, and without it the frontend's listener registration is refused
    // before it reaches Android — Back is owned, never delivered, in silence.
    const buildScript = readFileSync("src-tauri/build.rs", "utf8");
    expect(buildScript).toMatch(/\.plugin\(\s*"safe-back",/u);
    expect(buildScript).toContain('.commands(&["registerListener", "removeListener"])');
    const mobileCapability = readFileSync("src-tauri/capabilities/mobile.json", "utf8");
    expect(JSON.parse(mobileCapability).permissions).toContain("safe-back:default");
    expect(JSON.parse(mobileCapability).platforms).toContain("android");
    expect(safeBackPlugin).not.toContain(".goBack()");
    expect(safeBackPlugin).not.toContain("activity.onBackPressed()");
    expect(safeBackPlugin).not.toContain("isEnabled = false");
  });

  it("keeps Android root Back as frontend preparation then explicit activity exit", () => {
    const app = readFileSync("src/App.tsx", "utf8");
    const androidBack = readFileSync("src/androidBack.ts", "utf8");
    const commands = rustModuleSource("src-tauri/src/commands.rs");
    const lib = readFileSync("src-tauri/src/lib.rs", "utf8");
    const nativePlugin = readFileSync("src-tauri/src/android_safe_back.rs", "utf8");
    const defaultCapability = JSON.parse(
      readFileSync("src-tauri/capabilities/default.json", "utf8"),
    ) as { permissions: string[] };

    expect(app).toContain("finishActivity: exitAndroidActivity");
    expect(androidBack).toContain('import("@tauri-apps/plugin-process")');
    expect(androidBack).toContain("await exit(0)");
    expect(defaultCapability.permissions).toContain("process:allow-exit");
    expect(commands).toMatch(/pub\(crate\) fn tine_quit\([\s\S]*?app\.exit\(0\)/);
    expect(lib).toMatch(/generate_handler!\[[\s\S]*?\btine_quit,/);
    expect(lib).toContain("builder.plugin(android_safe_back::init())");
    expect(nativePlugin).toContain('Builder::new("safe-back")');
    expect(nativePlugin).toContain('register_android_plugin(PLUGIN_IDENTIFIER, "SafeBackPlugin")');
  });

  it("hands a safely prepared root close to Tauri's installed process exit API", async () => {
    const { exitAndroidActivity } = await import("./androidBack");
    const exit = vi.fn(async (_code?: number) => {});

    await exitAndroidActivity(async () => ({ exit }));

    expect(exit).toHaveBeenCalledWith(0);
  });
});
