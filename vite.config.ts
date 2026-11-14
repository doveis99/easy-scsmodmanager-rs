import { defineConfig } from "vite";

// The Tauri shell loads http://localhost:1420 in dev and ui/dist in a build.
export default defineConfig({
  root: "ui",
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true, target: "es2022" },
});
