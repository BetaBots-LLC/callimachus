// Browser-only entry for mock.html: install the fake Tauri runtime, boot the real app,
// then apply the scenario params that need the stores loaded. Never part of a build.
import "./mock-tauri";
import type { SettingsSection, SettingsSectionAlias, View } from "../store/ui";

const VIEWS: readonly View[] = [
  "today",
  "search",
  "chat",
  "projects",
  "stats",
  "settings",
  "report",
];
// The pre-merge ids ("providers", "knowledge") still open Settings > AI engine.
const SECTIONS: readonly (SettingsSection | SettingsSectionAlias)[] = [
  "general",
  "engine",
  "agents",
  "sources",
  "index",
  "export",
  "updates",
  "providers",
  "knowledge",
];

await import("../main");

const params = new URLSearchParams(window.location.search);
const view = params.get("view");
const section = params.get("section");
const { useUi } = await import("../store/ui");
const ui = useUi.getState();
if (section && (SECTIONS as readonly string[]).includes(section)) {
  ui.openSettings(section as SettingsSection | SettingsSectionAlias);
} else if (view && (VIEWS as readonly string[]).includes(view)) {
  ui.setView(view as View);
}
// onboarding=partial|done: the index is built, so the checklist only shows when opened.
const onboarding = params.get("onboarding");
if (onboarding === "partial" || onboarding === "done") {
  const { useChecklist } = await import("../store/checklist");
  useChecklist.getState().show();
}
// chatmode=ask: Chat opens in Ask mode; ask=<question> also sends it, for the answered state.
const chatMode = params.get("chatmode");
const ask = params.get("ask");
if (chatMode === "ask" || ask) {
  const { useChat } = await import("../store/chat");
  useChat.getState().setMode("ask");
  if (ask) void useChat.getState().send(ask);
}
