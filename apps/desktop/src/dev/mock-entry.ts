// Browser-only entry for mock.html: install the fake Tauri runtime, boot the real app,
// then apply the scenario params that need the stores loaded. Never part of a build.
import "./mock-tauri";
import type { SettingsSection, View } from "../store/ui";

const VIEWS: readonly View[] = [
  "search",
  "chat",
  "coach",
  "knowledge",
  "ask",
  "projects",
  "stats",
  "settings",
];
const SECTIONS: readonly SettingsSection[] = [
  "general",
  "providers",
  "agents",
  "sources",
  "index",
  "knowledge",
  "export",
  "updates",
];

await import("../main");

const params = new URLSearchParams(window.location.search);
const view = params.get("view");
const section = params.get("section");
const { useUi } = await import("../store/ui");
const ui = useUi.getState();
if (section && (SECTIONS as readonly string[]).includes(section)) {
  ui.openSettings(section as SettingsSection);
} else if (view && (VIEWS as readonly string[]).includes(view)) {
  ui.setView(view as View);
}
