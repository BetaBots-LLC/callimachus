import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// Builds the React webview UI into `media/` as a single, unhashed
// `webview.js` + `webview.css` pair that the extension host loads via
// `webview.asWebviewUri`. The host itself stays on `tsc` (see tsconfig.json).
//
// We reuse the desktop app's Tauri-free primitives (`ui/*`, Markdown) and its
// Tailwind v4 theme for visual parity — hence the `@desktop` alias and the
// `@source` directive in src/webview/index.css that lets Tailwind scan them.
export default defineConfig({
  // Assets are referenced through asWebviewUri, never an absolute server path.
  base: "./",
  plugins: [react(), tailwindcss()],
  resolve: {
    // Order matters: alias entries match first-wins by prefix, so the specific
    // "@/lib/utils" must precede the broad "@". The lifted desktop primitives
    // import `cn` from "@/lib/utils" *internally* (audited: no other "@/…"), and
    // would otherwise resolve against this webview's "@" → src/webview/lib/utils.
    alias: [
      {
        find: "@/lib/utils",
        replacement: path.resolve(__dirname, "../desktop/src/lib/utils.ts"),
      },
      { find: "@desktop", replacement: path.resolve(__dirname, "../desktop/src") },
      { find: "@", replacement: path.resolve(__dirname, "./src/webview") },
    ],
  },
  build: {
    outDir: "media",
    emptyOutDir: false, // media/ also holds the committed activity-bar icon.
    cssCodeSplit: false, // force a single webview.css.
    sourcemap: false, // would otherwise ship in the .vsix (media/** is included).
    target: "es2022",
    rollupOptions: {
      input: path.resolve(__dirname, "./src/webview/main.tsx"),
      output: {
        format: "es",
        entryFileNames: "webview.js",
        chunkFileNames: "webview-[name].js",
        assetFileNames: (asset) =>
          asset.names?.[0]?.endsWith(".css") ? "webview.css" : "assets/[name][extname]",
      },
    },
  },
});
