import { createFileRoute } from "@tanstack/react-router";
import { SITE_URL } from "@/lib/site";

const PATHS = [
  "/",
  "/download",
  "/desktop",
  "/vscode",
  "/cli",
  "/mcp",
  "/pricing",
  "/terms",
  "/privacy",
];

export const Route = createFileRoute("/sitemap.xml")({
  server: {
    handlers: {
      GET: () => {
        const urls = PATHS.map(
          (p) => `<url><loc>${SITE_URL}${p === "/" ? "/" : p}</loc></url>`,
        ).join("");
        const xml = `<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${urls}</urlset>`;
        return new Response(xml, {
          headers: { "Content-Type": "application/xml; charset=utf-8" },
        });
      },
    },
  },
});
