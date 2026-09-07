import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";
import { fileURLToPath } from "node:url";

// Separate config for the AST renderer tests: these import the .tsx render
// components and mount them into a jsdom DOM (client mode), so they need the
// Solid plugin + a DOM environment (the main vitest config is pure-TS / node).
export default defineConfig({
  plugins: [solid()],
  // Build-time constants injected by Vite's `define` in prod (vite.config.ts);
  // stub them so components that read them can be mounted in tests.
  define: {
    __BUILD_TIME__: JSON.stringify("1970-01-01T00:00:00.000Z"),
    __GIT_COMMIT__: JSON.stringify(""),
    __TINE_COMMUNITY_REGISTRY__: JSON.stringify(true),
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.tsx"],
    setupFiles: ["./src/testSetup.render.ts"],
    // Inline the Solid runtime AND any dependency that imports it. A package
    // left external resolves its own `solid-js`, which is a SECOND reactive
    // graph: its `onMount` never runs under our owner, so a virtualizer would
    // silently never attach to its scroll element. (Vite bundles both into one
    // module in a real build; only the test runner can externalize them.)
    server: {
      deps: { inline: ["solid-js", "@tanstack/solid-virtual", "@tanstack/virtual-core"] },
    },
  },
  resolve: {
    conditions: ["browser"],
    alias: [
      { find: /^solid-js$/, replacement: fileURLToPath(new URL("./node_modules/solid-js/dist/dev.js", import.meta.url)) },
      { find: /^solid-js\/store$/, replacement: fileURLToPath(new URL("./node_modules/solid-js/store/dist/dev.js", import.meta.url)) },
      { find: /^solid-js\/web$/, replacement: fileURLToPath(new URL("./node_modules/solid-js/web/dist/dev.js", import.meta.url)) },
    ],
  },
  ssr: {
    resolve: { conditions: ["browser"], externalConditions: ["browser"] },
  },
});
