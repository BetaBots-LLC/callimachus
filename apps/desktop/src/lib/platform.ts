// System-aware modifier key. In a Tauri webview the user agent reliably reports
// the host OS, so we resolve the "mod" key once at module load: ⌘ on macOS, Ctrl
// everywhere else. This is display-only — the key handlers accept
// `metaKey || ctrlKey`, so the shortcut works on every platform regardless.
export const isMac =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad/i.test(navigator.userAgent);

/** Display label for the platform modifier key: "⌘" on macOS, "Ctrl" elsewhere. */
export const MOD_KEY = isMac ? "⌘" : "Ctrl";
