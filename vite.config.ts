import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));

// Tauri expects a fixed port and no obfuscation
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
  },
  envPrefix: ["VITE_", "TAURI_"],
  resolve: {
    alias: {
      "@": resolve(__dirname, "src"),
    },
  },
  build: {
    target: "es2022",
    // Source maps are excluded from production builds: Tauri embeds `dist/`
    // into the native binary, so a 3 MB `.map` would ship in every installer
    // and exe for no end-user benefit (the source is local during development).
    // `vite dev` serves source maps regardless of this setting.
    sourcemap: false,
    minify: "esbuild",
  },
});