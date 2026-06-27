# App screenshots — capture spec

Screenshots shown on the marketing site (`/desktop`, via `ScreenshotGallery`). Drop the captured
files here, then add an entry in `src/content/screenshots.ts` (uncomment the matching block).

## How to capture

1. Run the desktop app with a real (non-sensitive) index: `pnpm desktop:dev`.
2. Use the **dark** theme — it matches the site's aesthetic.
3. Resize the window to a clean 16:10-ish shape (~1440×900).
4. macOS: `Cmd+Shift+4`, then `Space`, then click the window → captures the window at 2x retina.
5. Scrub anything private (repo paths, secrets, client names) before exporting.
6. Optimize to **webp** (keep it crisp, target < ~250 KB each):
   `cwebp -q 82 shot.png -o shot.webp`  (or `pnpm dlx @squoosh/cli --webp auto shot.png`)
7. Save with the names below, then enable the entry in `src/content/screenshots.ts`.

## Shot list (priority order)

| file              | screen                                              | span | caption                       |
| ----------------- | --------------------------------------------------- | ---- | ----------------------------- |
| `search.webp`     | Search results with source/project/tag filters      | 12   | fig. 1 — the reading room     |
| `projects.webp`   | Projects tab: decisions/gotchas/TODOs + LLM brief   | 7    | fig. 2 — per-project memory   |
| `chat.webp`       | In-app chat answering over the user's own history    | 5    | fig. 3 — ask your history     |
| `commits.webp`    | Thread → inferred git commits timeline              | 7    | fig. 4 — thread to commits    |
| `stats.webp`      | Stats dashboard: per-source counts + spend by model | 5    | fig. 5 — know your corpus     |

## Specs

- **Format:** webp (PNG source ok; convert before committing).
- **Resolution:** 2x retina; ~2880×1800 source, displayed responsive.
- **Aspect:** keep consistent (16:10) so the grid stays tidy.
- **Theme:** dark UI.
- **Alt text:** describe the screen — it's the SEO/accessibility text (set in `screenshots.ts`).

`fig. 1` currently falls back to `/hero.png` until `search.webp` lands.
