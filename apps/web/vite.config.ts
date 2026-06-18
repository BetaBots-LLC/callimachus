import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import { nitro } from "nitro/vite";
import viteReact from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";
import path from "node:path";

// TanStack Start (Vite) + Nitro for the server build. Nitro produces
// `.output/server/index.mjs` (the `start` script) and auto-detects the host —
// on Vercel (the VERCEL env) it emits the Build Output API dir, so no preset is
// hard-coded and no vercel.json is needed. React's plugin must come after Start's.
export default defineConfig({
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  plugins: [tanstackStart(), nitro(), viteReact(), tailwindcss()],
});
