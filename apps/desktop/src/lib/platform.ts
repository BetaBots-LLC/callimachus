// Host-OS detection. In a Tauri webview the user agent reliably reports the host OS
// (WKWebView on macOS, WebView2 on Windows, WebKitGTK on Linux), so we resolve it once
// at module load instead of round-tripping to the backend.
const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";

export const IS_MAC = /Mac|iPhone|iPad/i.test(ua);
export const IS_WINDOWS = /Windows/i.test(ua);
export const IS_LINUX = !IS_MAC && !IS_WINDOWS && /Linux|X11/i.test(ua);

/** @deprecated Prefer `IS_MAC`; kept so existing imports keep working. */
export const isMac = IS_MAC;

/**
 * Best guess at whether the backend can open a thread in a terminal CLI ("Resume",
 * "Open in CLI"), used only until `api.cliLaunchSupported()` answers. The real answer
 * comes from the backend (agent/cli_resume.rs `launch_supported`): macOS and Windows
 * always, Linux only when a terminal emulator is found.
 */
export const CAN_LAUNCH_CLI_FALLBACK = IS_MAC || IS_WINDOWS;

// System-aware modifier key: ⌘ on macOS, Ctrl everywhere else. Display-only; the key
// handlers accept `metaKey || ctrlKey`, so shortcuts work on every platform regardless.
/** Display label for the platform modifier key: "⌘" on macOS, "Ctrl" elsewhere. */
export const MOD_KEY = IS_MAC ? "⌘" : "Ctrl";
