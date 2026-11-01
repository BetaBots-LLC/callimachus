import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      // import.meta.dirname (not __dirname) works with Vite's native ESM config loader.
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },

  build: {
    rolldownOptions: {
      output: {
        // Syntax highlighting (highlight.js grammars via lowlight) is the largest single
        // dependency. Keep it in its own chunk so the entry chunk stays small and the
        // highlighter is parsed and cached separately from app code.
        codeSplitting: {
          groups: [
            {
              name: "highlight",
              // Matches the package dir inside pnpm's store too (…/node_modules/<pkg>/).
              test: /[\\/]node_modules[\\/](highlight\.js|lowlight|rehype-highlight)[\\/]/,
            },
          ],
        },
      },
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
