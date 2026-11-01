// Opt-in, anonymous usage analytics: thin, typed wrappers over the Rust telemetry
// commands (src-tauri/src/telemetry.rs). The webview never talks to PostHog itself; it
// only hands a closed set of events to Rust, which validates every name and property
// against the same allowlists, drops anything unexpected, and sends nothing at all
// unless the user opted in. Keep the unions below in sync with the Rust allowlists.
import { invoke } from "@tauri-apps/api/core";
import type { SourceKind } from "./api";

export type Consent = "granted" | "denied" | "unset";

/** What `telemetry_state` / `set_telemetry` return. */
export interface TelemetryStatus {
  consent: Consent;
  /** Events are being sent right now. */
  enabled: boolean;
  /** This build can send at all (release build with a token, no env opt-out). False in
   *  `tauri dev`, source builds and forks, where the consent prompt stays hidden. */
  available: boolean;
  /** Disabled by DO_NOT_TRACK=1 or CALLIMACHUS_TELEMETRY=0. */
  envOptOut: boolean;
  /** Random install id, shown in Settings while consent is granted. */
  anonId: string | null;
}

/** Where the privacy policy lists exactly what is collected. The fragment is the anchor
 *  LegalPage derives from the section heading "Usage analytics in the desktop app". */
export const PRIVACY_URL = "https://callimachus.app/privacy#usage-analytics-in-the-desktop-app";

/** Settings whose change (never their value) may be reported. Mirrors `SETTING_KEYS`. */
export type SettingKey =
  | "close_to_tray"
  | "auto_distill"
  | "proactive_recall"
  | "knowledge_engine"
  | "theme"
  | "api_key"
  | "provider_base_url"
  | "vault_dir"
  | "synth_engine";

/** Error classes. Raw error text never leaves the machine. Mirrors `ERROR_KINDS`. */
export type ErrorKind =
  | "model_unavailable"
  | "auth_rejected"
  | "missing_key"
  | "rate_limited"
  | "network"
  | "index_failed"
  | "distill_failed"
  | "export_failed"
  | "update_failed"
  | "unknown";

/** Agents the app installs into. Mirrors `AGENTS`. */
export type IntegrationAgent = "claude_code" | "codex" | "cursor" | "gemini";

/**
 * Event name to props. Mirrors `Event::from_name_props` in Rust, which rejects any
 * other name or key. Counts and durations are sent raw here and bucketed in Rust.
 * `provider` / `engine` are provider ids (e.g. "anthropic", "claude-cli"); Rust
 * replaces anything not on its allowlist with "other".
 */
export interface TelemetryEvents {
  index_completed: { sources: number; threads: number; duration_ms: number };
  search_performed: { hybrid: boolean; has_filters: boolean; results: number };
  thread_opened: { source: SourceKind };
  chat_message_sent: { provider: string };
  knowledge_enabled: { engine: string };
  distill_completed: { threads: number };
  integration_installed: { agent: IntegrationAgent };
  obsidian_exported: { synthesized: boolean };
  // No update_installed: Rust records it after the relaunch and rejects it from here.
  setting_changed: { key: SettingKey };
  error_shown: { kind: ErrorKind };
}

export type TelemetryEventName = keyof TelemetryEvents;

export const telemetryState = () => invoke<TelemetryStatus>("telemetry_state");

export const setTelemetry = (granted: boolean) =>
  invoke<TelemetryStatus>("set_telemetry", { granted });

/**
 * Fire-and-forget. Safe to call unconditionally: Rust drops the event when consent is
 * not granted or the build has no token. Never awaits, never throws.
 */
export function track<N extends TelemetryEventName>(name: N, props: TelemetryEvents[N]): void {
  invoke("track_event", { name, props }).catch(() => {
    /* telemetry must never surface an error to the user */
  });
}

/** Send anything still buffered. Await it right before `relaunch()` (the restart path
 *  skips the normal exit flush). Resolves within ~1.5 s even when offline. */
export async function flushTelemetry(): Promise<void> {
  try {
    await invoke("telemetry_flush");
  } catch {
    /* best effort */
  }
}

// ---------------------------------------------------------------------------
// Search: one event per settled query, not one per debounced keystroke.
// ---------------------------------------------------------------------------

/** How long a query must sit unchanged before it counts as "performed". */
const SEARCH_SETTLE_MS = 1500;

type SearchProps = TelemetryEvents["search_performed"];

let pendingSearch: {
  key: string;
  props: SearchProps;
  timer: ReturnType<typeof setTimeout>;
} | null = null;
let lastSearchKey: string | null = null;

/**
 * Report that results for a query arrived. Call it every time the results list
 * resolves; it sends `search_performed` only once the same query has settled for
 * 1.5 s (or a result is opened, see `commitSearch`). `key` is any string that
 * identifies the query + filters (e.g. `JSON.stringify(queryKey)`). It is used only
 * in memory for de-duplication and is never sent.
 */
export function noteSearch(key: string, props: SearchProps): void {
  if (pendingSearch) clearTimeout(pendingSearch.timer);
  pendingSearch = null;
  // A re-render or refetch of the query we already reported.
  if (key === lastSearchKey) return;
  pendingSearch = { key, props, timer: setTimeout(commitSearch, SEARCH_SETTLE_MS) };
}

/** Send the pending search now (call when the user opens a result). */
export function commitSearch(): void {
  if (!pendingSearch) return;
  clearTimeout(pendingSearch.timer);
  lastSearchKey = pendingSearch.key;
  track("search_performed", pendingSearch.props);
  pendingSearch = null;
}

// ---------------------------------------------------------------------------
// Error classification: the error *class* is reported, never its text.
// ---------------------------------------------------------------------------

/**
 * Classify a raw provider / backend error into an `ErrorKind`. Uses the same keyword
 * branches, in the same order, as `humanizeApiError` in errors.ts, so the class matches
 * the message the user actually saw. Pass `fallback` for errors from a known surface
 * (e.g. "export_failed") so unmatched text still lands in a useful bucket.
 */
export function classifyError(raw: unknown, fallback: ErrorKind = "unknown"): ErrorKind {
  const low = String(raw ?? "").toLowerCase();
  const has = (...keys: string[]) => keys.some((k) => low.includes(k));
  if (has("not_found", "404", "not available", "does not exist", "no such model")) {
    return "model_unavailable";
  }
  if (has("invalid_api_key", "401", "403", "unauthorized", "authentication", "permission")) {
    return "auth_rejected";
  }
  if (has("missing", "no api key", "api key")) return "missing_key";
  if (has("429", "rate limit", "overloaded", "529", "quota", "insufficient_quota")) {
    return "rate_limited";
  }
  if (
    has(
      "connection",
      "dns",
      "timed out",
      "timeout",
      "error sending request",
      "tcp connect",
      "failed to connect",
      "connrefused",
    )
  ) {
    return "network";
  }
  return fallback;
}

/**
 * Report that an error was shown to the user: its class only, never its text. Call it
 * once per displayed error (a mutation's `onError`, a catch that sets visible error
 * state), not from render, so a re-render can't report the same error twice.
 */
export function trackError(raw: unknown, fallback: ErrorKind = "unknown"): void {
  track("error_shown", { kind: classifyError(raw, fallback) });
}
