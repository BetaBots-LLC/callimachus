// SSR data source for downloads. A server function fetches the latest GitHub
// release at request time and maps its assets to a typed shape, so the version
// and per-OS links are server-rendered into the HTML (crawlable, no flash). It
// caches briefly to respect GitHub's rate limit and falls back to stable
// `releases/latest/download` URLs if the API is unreachable, so the page never
// breaks.

import { createServerFn } from "@tanstack/react-start";
import { getRequestHeader } from "@tanstack/react-start/server";
import { REPO } from "@/lib/site";
import { type OsKey, osFromUserAgent } from "@/lib/os-detect";

export type ReleaseAssets = Record<OsKey, string>;

export interface Release {
  version: string;
  publishedAt: string | null;
  assets: ReleaseAssets;
  /** True when we served the static fallback (the live API was unavailable). */
  fallback: boolean;
}

// Fallback when the GitHub API is unreachable. Asset names embed the version, so a
// fixed `releases/latest/download/<name>` URL can't be hardcoded reliably — send
// users to the latest release page instead, which never 404s.
const RELEASES_LATEST = `https://github.com/${REPO}/releases/latest`;
function staticAssets(): ReleaseAssets {
  return {
    mac_arm: RELEASES_LATEST,
    win: RELEASES_LATEST,
    linux_appimage: RELEASES_LATEST,
    linux_deb: RELEASES_LATEST,
  };
}

interface GhAsset {
  name: string;
  browser_download_url: string;
}

let cache: { at: number; data: Release } | null = null;
const TTL_MS = 5 * 60_000;

export const getLatestRelease = createServerFn({ method: "GET" }).handler(
  async (): Promise<Release> => {
    if (cache && Date.now() - cache.at < TTL_MS) return cache.data;

    try {
      const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
        headers: {
          Accept: "application/vnd.github+json",
          "User-Agent": "callimachus-web",
          ...(process.env.GITHUB_TOKEN
            ? { Authorization: `Bearer ${process.env.GITHUB_TOKEN}` }
            : {}),
        },
      });
      if (!res.ok) throw new Error(`GitHub API ${res.status}`);

      const json = (await res.json()) as {
        tag_name?: string;
        published_at?: string;
        assets?: GhAsset[];
      };
      const assets = json.assets ?? [];
      const url = (re: RegExp, fallback: string) =>
        assets.find((a) => re.test(a.name))?.browser_download_url ?? fallback;
      const fb = staticAssets();

      const data: Release = {
        version: (json.tag_name ?? "").replace(/^v/, "") || "latest",
        publishedAt: json.published_at ?? null,
        fallback: false,
        assets: {
          // macOS is Apple Silicon only (no x86_64 ONNX prebuilt — see build.yml).
          mac_arm: url(/(aarch64|arm64).*\.dmg$/i, fb.mac_arm),
          // Serve Tauri's NSIS installer `*_x64-setup.exe` — a per-user installer
          // (no admin prompt), the better default than the WiX `*_en-US.msi` (which
          // also ships, for enterprise/admin installs). Both self-update via the
          // updater's install-type-aware keys.
          win: url(/-setup\.exe$/i, fb.win),
          linux_appimage: url(/\.AppImage$/i, fb.linux_appimage),
          linux_deb: url(/\.deb$/i, fb.linux_deb),
        },
      };
      cache = { at: Date.now(), data };
      return data;
    } catch {
      // Serve last good data if we have it, else static fallback URLs.
      return (
        cache?.data ?? {
          version: "latest",
          publishedAt: null,
          fallback: true,
          assets: staticAssets(),
        }
      );
    }
  },
);

/** The visitor's OS, detected server-side from the request User-Agent — so the
 * primary download CTA is correct in the SSR'd HTML with no client work. */
export const getPrimaryOs = createServerFn({ method: "GET" }).handler(
  async (): Promise<OsKey> => osFromUserAgent(getRequestHeader("user-agent") ?? ""),
);

/** Everything a download CTA needs, for use directly as a route `loader`. */
export async function downloadData(): Promise<{ release: Release; primaryOs: OsKey }> {
  const [release, primaryOs] = await Promise.all([getLatestRelease(), getPrimaryOs()]);
  return { release, primaryOs };
}
