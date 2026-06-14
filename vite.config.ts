import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Tauri expects a fixed port and serves the built assets from dist/.
export default defineConfig({
  plugins: [solid()],
  clearScreen: false,
  server: {
    port: 5181,
    strictPort: true,
  },
  build: {
    target: "esnext",
    outDir: "dist",
  },
});
