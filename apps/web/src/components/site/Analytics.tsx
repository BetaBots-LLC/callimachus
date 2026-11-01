import type { ReactNode } from "react";
import { PostHogProvider } from "@posthog/react";
import { POSTHOG_KEY, POSTHOG_OPTIONS } from "@/lib/analytics";

/**
 * Wraps the app in PostHog, cookieless. With `apiKey` + `options` the provider calls
 * `posthog.init` in an effect, so it only ever runs in the browser; server rendering
 * just provides the (uninitialized) context. Without a token (local dev, previews,
 * forks) children render unwrapped and nothing loads or sends.
 */
export function AnalyticsProvider({ children }: { children: ReactNode }) {
  if (!POSTHOG_KEY) return children;
  return (
    <PostHogProvider apiKey={POSTHOG_KEY} options={POSTHOG_OPTIONS}>
      {children}
    </PostHogProvider>
  );
}
