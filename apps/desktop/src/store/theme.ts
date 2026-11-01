import { create } from "zustand";
import { track } from "../lib/telemetry";

export type Theme = "light" | "dark";

function apply(theme: Theme) {
  document.documentElement.classList.toggle("dark", theme === "dark");
}

// Apply the persisted theme at module load (before first paint) — no useEffect.
const stored = (localStorage.getItem("theme") as Theme | null) ?? "dark";
apply(stored);

interface ThemeState {
  theme: Theme;
  toggle: () => void;
}

export const useTheme = create<ThemeState>((set) => ({
  theme: stored,
  toggle: () =>
    set((s) => {
      const theme: Theme = s.theme === "dark" ? "light" : "dark";
      localStorage.setItem("theme", theme);
      apply(theme);
      // Every entry point (header button, command palette) goes through here.
      track("setting_changed", { key: "theme" });
      return { theme };
    }),
}));
