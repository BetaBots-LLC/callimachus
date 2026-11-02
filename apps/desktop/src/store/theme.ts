import { create } from "zustand";
import { track } from "../lib/telemetry";

/** "system" follows the OS appearance and tracks it live. */
export type Theme = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

const media =
  typeof window !== "undefined" && "matchMedia" in window
    ? window.matchMedia("(prefers-color-scheme: dark)")
    : null;

function resolve(theme: Theme): ResolvedTheme {
  if (theme === "system") return media?.matches ? "dark" : "light";
  return theme;
}

function apply(theme: Theme) {
  document.documentElement.classList.toggle("dark", resolve(theme) === "dark");
}

function readStored(): Theme {
  const v = localStorage.getItem("theme");
  return v === "light" || v === "dark" || v === "system" ? v : "dark";
}

// Apply the persisted theme at module load (before first paint), no useEffect.
const stored = readStored();
apply(stored);

interface ThemeState {
  theme: Theme;
  /** What is on screen right now (`theme` with "system" resolved). */
  resolved: ResolvedTheme;
  setTheme: (theme: Theme) => void;
  /** Flip between light and dark (the header button, the command palette). From "system"
   *  it pins the opposite of what is showing. */
  toggle: () => void;
}

export const useTheme = create<ThemeState>((set, get) => ({
  theme: stored,
  resolved: resolve(stored),
  setTheme: (theme) => {
    if (theme === get().theme) return;
    localStorage.setItem("theme", theme);
    apply(theme);
    // Every entry point (Settings, header button, command palette) goes through here.
    track("setting_changed", { key: "theme" });
    set({ theme, resolved: resolve(theme) });
  },
  toggle: () => get().setTheme(get().resolved === "dark" ? "light" : "dark"),
}));

// With "system", follow the OS when it switches (sunset, a manual toggle).
media?.addEventListener("change", () => {
  const { theme } = useTheme.getState();
  if (theme !== "system") return;
  apply(theme);
  useTheme.setState({ resolved: resolve(theme) });
});
