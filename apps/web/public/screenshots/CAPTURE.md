# App screenshots: capture spec

Screenshots shown on the marketing site (`/` and `/desktop`, via `ScreenshotGallery`). Drop the
captured files here, then add or update the entry in `src/content/screenshots.ts`.

## How to capture

1. Run the desktop app with a real (non-sensitive) index: `pnpm desktop:dev`.
2. Use the **dark** theme: it matches the site's aesthetic.
3. Resize the window to a clean 16:10-ish shape (~1440×900).
4. macOS: `Cmd+Shift+4`, then `Space`, then click the window to capture it at 2x retina.
5. Scrub anything private (repo paths, secrets, client names) before exporting.
6. Optimize to **webp** (keep it crisp, target < ~250 KB each):
   `cwebp -q 82 shot.png -o shot.webp`  (or `pnpm dlx @squoosh/cli --webp auto shot.png`)
7. Save with the names below, then update the entry in `src/content/screenshots.ts` (caption
   and alt text describe what the new image shows).

## Shot list (priority order)

Every plate captured before the Today / Search / Chat / Projects tab bar shows the retired
Coach, Knowledge and Ask tabs, so all of these need a new capture.

| file             | screen                                                                          | caption                       |
| ---------------- | ------------------------------------------------------------------------------- | ----------------------------- |
| `/hero.png`      | Home hero: the Today view, sessions by repo since the last open, commits, Copy as standup (replaces the brand banner; update its alt and size) | the catalogue (hero label, not a gallery figure) |
| `search.webp`    | Search scoped to a repo and 7 days, a "Decisions and gotchas" group on top      | fig. 1 · the reading room     |
| `projects.webp`  | The Project page: picker, Recent sessions, Memory, Snapshots                    | fig. 2 · per-project memory   |
| `ask.webp`       | Chat in Ask mode: a cited answer with [thread N] chips                          | fig. 3 · ask your history     |
| `agents.webp`    | The Project page's What agents see: the hook text and the AGENTS.md block       | fig. 4 · what agents see (replaces `coach.webp`) |
| `stats.webp`     | Stats: activity heatmap, "What Callimachus did for your agents", spend          | fig. 5 · know your corpus     |
| `chat.webp`      | Chat on a keyless CLI engine, the Chat / Ask control in the header              | fig. 6 · talk to your archive |

The hero is the Today view, so the gallery has no separate Today plate. The home page labels
the hero "fig. 1 · the catalogue" (`src/routes/index.tsx`), which collides with the gallery's
fig. 1 on the same page; when the hero is reshot, give it a label outside the figure numbers.

## Specs

- **Format:** webp (PNG source ok; convert before committing).
- **Resolution:** 2x retina; ~2880×1800 source, displayed responsive.
- **Aspect:** keep consistent (16:10) so the grid stays tidy.
- **Theme:** dark UI.
- **Alt text:** describe the screen; it's the SEO and accessibility text (set in `screenshots.ts`).
