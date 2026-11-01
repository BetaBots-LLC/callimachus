// Website analytics (PostHog), configured for the privacy promise on /privacy:
// cookieless (no cookies, localStorage or sessionStorage), no person profiles, no
// autocapture, no session replay, no remote scripts or flags. What is collected:
// pageviews, page leaves, and the one custom event below. This is the single event
// catalog for the site; add new events here (and to the privacy policy) only.
import { usePostHog } from "@posthog/react";
import type { PostHogConfig } from "posthog-js";
import { useCallback } from "react";
import type { OsKey } from "@/lib/os-detect";

const env = (v: string | undefined) => v?.trim() || undefined;

/** Project token. Unset (local dev, previews, forks) means analytics are fully off. */
export const POSTHOG_KEY = env(import.meta.env.VITE_PUBLIC_POSTHOG_KEY as string | undefined);

const POSTHOG_HOST =
  env(import.meta.env.VITE_PUBLIC_POSTHOG_HOST as string | undefined) ?? "https://us.i.posthog.com";

/**
 * Init options. Cookieless mode also has to be switched on in the PostHog project
 * (Settings > Project > Web analytics > Enable cookieless tracking), otherwise
 * PostHog silently drops these events.
 */
export const POSTHOG_OPTIONS: Partial<PostHogConfig> = {
  api_host: POSTHOG_HOST,
  // A snapshot date the installed posthog-js types accept (CONFIG_DEFAULTS_DATES).
  defaults: "2026-08-30",
  cookieless_mode: "always",
  person_profiles: "never",
  // SPA navigations count as pageviews; page leaves give bounce rate and time on page.
  capture_pageview: "history_change",
  capture_pageleave: "if_capture_pageview",
  // Everything below is off so only the events named above are ever sent.
  autocapture: false,
  rageclick: false,
  capture_dead_clicks: false,
  capture_heatmaps: false,
  capture_exceptions: false,
  capture_performance: false,
  disable_session_recording: true,
  disable_surveys: true,
  disable_product_tours: true,
  disable_conversations: true,
  // No lazily loaded remote scripts and no /flags call, which also means no remote
  // config can turn any of the above back on.
  disable_external_dependency_loading: true,
  advanced_disable_flags: true,
};

/** Where on the site a download link lives. */
export type DownloadPlacement =
  | "hero"
  | "closing_cta"
  | "download_page"
  | "desktop_page"
  | "download_matrix";

/** The site's custom events. Nothing identifying: an OS key and a placement. */
export interface WebEvents {
  download_clicked: { os: OsKey; placement: DownloadPlacement };
}

/**
 * Typed capture. A no-op when no project token is configured. Sends immediately via
 * sendBeacon so the event survives a download link that navigates away.
 */
export function useTrack() {
  const posthog = usePostHog();
  return useCallback(
    <N extends keyof WebEvents>(name: N, props: WebEvents[N]) => {
      if (!POSTHOG_KEY) return;
      posthog?.capture(name, props, { send_instantly: true, transport: "sendBeacon" });
    },
    [posthog],
  );
}
