import { defineConfig } from "vite";

// The UI is served by the Rust app itself (window + tablets), so `npm run dev`
// proxies the WebSocket to a running app on :7878 for fast UI iteration.
export default defineConfig({
  clearScreen: false,
  build: { outDir: "dist", emptyOutDir: true, target: ["es2019", "safari12"], assetsInlineLimit: 0 },
  server: {
    port: 5173,
    proxy: { "/ws": { target: "ws://127.0.0.1:7878", ws: true } },
  },
});
