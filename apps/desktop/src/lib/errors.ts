// Turn a raw provider/genai error string into a short, actionable message.
const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

export function humanizeApiError(raw: string, provider: string, model: string): string {
  const s = raw || "";
  const low = s.toLowerCase();
  const has = (...keys: string[]) => keys.some((k) => low.includes(k));
  // The provider's own human message, if the body carried one.
  const detail = s.match(/"message"\s*:\s*"([^"]+)"/)?.[1];
  const name = cap(provider);

  if (has("not_found", "404", "not available", "does not exist", "no such model")) {
    return `Model "${model}" isn't available on ${name}${detail ? ` — ${detail}` : "."} Pick another from the dropdown.`;
  }
  if (has("invalid_api_key", "401", "403", "unauthorized", "authentication", "permission")) {
    return `${name} rejected the request — check your API key in Settings.${detail ? ` (${detail})` : ""}`;
  }
  if (has("missing", "no api key", "api key")) {
    return `No ${name} API key set. Add one in Settings.`;
  }
  if (has("429", "rate limit", "overloaded", "529", "quota", "insufficient_quota")) {
    return `${name} is rate-limited or out of quota — wait a moment and retry.${detail ? ` (${detail})` : ""}`;
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
    return provider === "ollama"
      ? "Can't reach Ollama — is it running? (ollama serve)"
      : `Can't reach ${name} — check your internet connection.`;
  }
  // Fallback: the provider message, else the first line, trimmed.
  return detail || s.split("\n")[0].slice(0, 300) || "Something went wrong.";
}
