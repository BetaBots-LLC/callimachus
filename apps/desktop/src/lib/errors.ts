import { classifyError } from "./telemetry";

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/**
 * Turn a raw provider/genai error string into a short, actionable message. The keyword
 * matching lives in `classifyError` (lib/telemetry.ts), so the message shown here and the
 * error class reported by `trackError` can never disagree.
 */
export function humanizeApiError(raw: string, provider: string, model: string): string {
  const s = raw || "";
  // The provider's own human message, if the body carried one.
  const detail = s.match(/"message"\s*:\s*"([^"]+)"/)?.[1];
  // Underscored ids (e.g. ollama_cloud) don't title-case cleanly — give it its real label.
  const name = provider === "ollama_cloud" ? "Ollama Cloud" : cap(provider);

  switch (classifyError(s)) {
    case "model_unavailable":
      return `Model "${model}" isn't available on ${name}${detail ? ` — ${detail}` : "."} Pick another from the dropdown.`;
    case "auth_rejected":
      return `${name} rejected the request — check your API key in Settings.${detail ? ` (${detail})` : ""}`;
    case "missing_key":
      return `No ${name} API key set. Add one in Settings.`;
    case "rate_limited":
      return `${name} is rate-limited or out of quota — wait a moment and retry.${detail ? ` (${detail})` : ""}`;
    case "network":
      return provider === "ollama"
        ? "Can't reach Ollama — is it running? (ollama serve)"
        : `Can't reach ${name} — check your internet connection.`;
    default:
      // Fallback: the provider message, else the first line, trimmed.
      return detail || s.split("\n")[0].slice(0, 300) || "Something went wrong.";
  }
}

/**
 * A readable message for a failed API-key save. Keys go to the OS keychain; on Linux
 * that needs a running Secret Service (gnome-keyring or KWallet), and minimal window
 * managers, some KDE setups and WSL have none, which surfaces as a D-Bus error.
 */
export function describeKeySaveError(err: unknown): string {
  const raw = String(err instanceof Error ? err.message : err).replace(/^Error:\s*/, "");
  const noSecretService = /secret ?service|d-?bus|org\.freedesktop|no such interface/i.test(raw);
  if (noSecretService) {
    return "Couldn't save the key: no keyring service is running. Start gnome-keyring or KWallet (any Secret Service provider), then try again.";
  }
  return `Couldn't save the key: ${raw || "unknown error"}`;
}
