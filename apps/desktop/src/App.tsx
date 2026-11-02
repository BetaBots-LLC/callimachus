import { lazy, Suspense, useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import { BarChart3, Moon, Settings, Sun } from "lucide-react";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { SearchBar } from "./components/SearchBar";
import { ResultsList } from "./components/ResultsList";
import { ThreadView } from "./components/ThreadView";
import { KnowledgeGate } from "./components/KnowledgeGate";
import { Onboarding } from "./components/Onboarding";
import { CommandPalette } from "./components/CommandPalette";
import { TelemetryConsent } from "./components/TelemetryConsent";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Loading } from "./components/Loading";
import { preloadMarkdown } from "./components/LazyMarkdown";
import { Button } from "@/components/ui/button";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { api } from "./lib/api";
import { MOD_KEY } from "./lib/platform";
import { useUi, type View } from "./store/ui";
import { useTheme } from "./store/theme";

// Only the Search view (results + thread) is needed for first paint. The other views load
// on first visit, which keeps their code out of the startup bundle the webview has to
// parse behind the splash (most noticeable on low-end Windows and Linux machines).
const loadChat = () => import("./components/ChatView");
const loadCoach = () => import("./components/CoachView");
const loadKnowledge = () => import("./components/KnowledgeView");
const loadAsk = () => import("./components/AskView");
const loadProjects = () => import("./components/ProjectMemoryView");
const loadStats = () => import("./components/StatsView");
const loadSettings = () => import("./components/SettingsView");
const ChatView = lazy(() => loadChat().then((m) => ({ default: m.ChatView })));
const CoachView = lazy(() => loadCoach().then((m) => ({ default: m.CoachView })));
const KnowledgeView = lazy(() => loadKnowledge().then((m) => ({ default: m.KnowledgeView })));
const AskView = lazy(() => loadAsk().then((m) => ({ default: m.AskView })));
const ProjectMemoryView = lazy(() =>
  loadProjects().then((m) => ({ default: m.ProjectMemoryView })),
);
const StatsView = lazy(() => loadStats().then((m) => ({ default: m.StatsView })));
const SettingsView = lazy(() => loadSettings().then((m) => ({ default: m.SettingsView })));

/** Warm the lazy view chunks once the app is idle, so the first visit to a tab doesn't
 *  flash a loading state. Runs after first paint, so startup is unaffected. */
const PREFETCH_DELAY_MS = 2_000;
function usePrefetchViews() {
  useEffect(() => {
    // The markdown stack is split out of the entry only to keep it off the splash path,
    // and opening a thread is the first thing most people do, so fetch it right after
    // first paint rather than with the other views.
    preloadMarkdown().catch(() => {}); // a real failure resurfaces on render, inside the boundary
    const id = setTimeout(() => {
      for (const load of [
        loadChat,
        loadSettings,
        loadStats,
        loadCoach,
        loadKnowledge,
        loadAsk,
        loadProjects,
      ]) {
        load().catch(() => {}); // a real failure resurfaces on visit, inside the boundary
      }
    }, PREFETCH_DELAY_MS);
    return () => clearTimeout(id);
  }, []);
}

// Content views shown as labeled tabs. Utilities (Stats, Settings) live as right-side icons.
const TABS: { id: View; label: string }[] = [
  { id: "search", label: "Search" },
  { id: "chat", label: "Chat" },
  { id: "coach", label: "Coach" },
  { id: "knowledge", label: "Knowledge" },
  { id: "ask", label: "Ask" },
  { id: "projects", label: "Projects" },
];

function App() {
  usePrefetchViews();
  const view = useUi((s) => s.view);
  const setView = useUi((s) => s.setView);
  const setCommandOpen = useUi((s) => s.setCommandOpen);
  const theme = useTheme((s) => s.resolved);
  const toggleTheme = useTheme((s) => s.toggle);
  const { data: stats } = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  // Knowledge powers these three views. The tabs stay visible even when it's off (each
  // shows a teaser + Enable CTA) so the features are discoverable, not hidden behind a flag.
  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const on = !!knowledge.data?.enabled;
  // DEV-only preview of the first-run screen on a populated index:
  //   VITE_ONBOARD=1 pnpm desktop:dev   (or devtools: localStorage.setItem("cal:onboard","1"))
  const forceOnboard =
    import.meta.env.DEV &&
    (import.meta.env.VITE_ONBOARD === "1" || localStorage.getItem("cal:onboard") === "1");
  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <header className="flex items-center justify-between border-b px-4 py-2">
        <div className="flex items-center gap-3">
          <span className="font-semibold tracking-tight">Callimachus</span>
          <nav className="flex gap-1">
            {TABS.map((t) => (
              <Button
                key={t.id}
                size="sm"
                variant={view === t.id ? "secondary" : "ghost"}
                onClick={() => setView(t.id)}
              >
                {t.label}
              </Button>
            ))}
          </nav>
        </div>
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={() => setCommandOpen(true)}
            title="Command palette"
            aria-label="Open command palette"
            className="mr-1 hidden cursor-pointer items-center transition-opacity hover:opacity-80 sm:flex"
          >
            <KbdGroup>
              <Kbd>{MOD_KEY}</Kbd>
              <Kbd>K</Kbd>
            </KbdGroup>
          </button>
          <TooltipProvider delay={200}>
            <Tooltip>
              <TooltipTrigger
                render={
                  <Button
                    size="icon"
                    variant={view === "stats" ? "secondary" : "ghost"}
                    onClick={() => setView("stats")}
                    aria-label="Stats"
                  />
                }
              >
                <BarChart3 />
              </TooltipTrigger>
              <TooltipContent>
                Stats
                {stats
                  ? ` · ${stats.threads.toLocaleString()} threads, ${stats.messages.toLocaleString()} messages`
                  : ""}
              </TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger
                render={
                  <Button
                    size="icon"
                    variant={view === "settings" ? "secondary" : "ghost"}
                    onClick={() => setView("settings")}
                    aria-label="Settings"
                  />
                }
              >
                <Settings />
              </TooltipTrigger>
              <TooltipContent>Settings</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger
                render={
                  <Button
                    size="icon"
                    variant="ghost"
                    onClick={toggleTheme}
                    aria-label="Toggle theme"
                  />
                }
              >
                {theme === "dark" ? <Sun /> : <Moon />}
              </TooltipTrigger>
              <TooltipContent>{theme === "dark" ? "Light mode" : "Dark mode"}</TooltipContent>
            </Tooltip>
          </TooltipProvider>
        </div>
      </header>

      {/* Keyed by view: a crash in one tab shows an inline fallback there, and switching
          tabs remounts a fresh boundary so the rest of the app keeps working. */}
      <ErrorBoundary key={view} scope="view">
        <Suspense fallback={<Loading label="Loading…" className="flex-1" />}>
          <div className="flex min-h-0 flex-1 flex-col">
            {view === "search" &&
              ((stats && stats.threads === 0) || forceOnboard ? (
                <Onboarding />
              ) : (
                <>
                  <SearchBar />
                  <main className="grid min-h-0 flex-1 grid-cols-[minmax(340px,420px)_1fr]">
                    <section className="min-h-0 border-r">
                      <ResultsList />
                    </section>
                    <section className="min-h-0">
                      <ThreadView />
                    </section>
                  </main>
                </>
              ))}
            {view === "chat" && <ChatView />}
            {view === "coach" && <CoachView />}
            {view === "knowledge" && (
              <KnowledgeGate
                enabled={on}
                feature="Knowledge"
                blurb="Decisions, gotchas, and open TODOs distilled from your past sessions, with cross-thread recall."
              >
                <KnowledgeView />
              </KnowledgeGate>
            )}
            {view === "ask" && (
              <KnowledgeGate
                enabled={on}
                feature="Ask your history"
                blurb="Ask a question and get a synthesized, cited answer drawn from your own past sessions."
              >
                <AskView />
              </KnowledgeGate>
            )}
            {view === "projects" && (
              <KnowledgeGate
                enabled={on}
                feature="Project Memory"
                blurb="Each repo's decisions, gotchas, and TODOs aggregated into durable memory you (and your agents) can reuse."
              >
                <ProjectMemoryView />
              </KnowledgeGate>
            )}
            {view === "stats" && <StatsView />}
            {view === "settings" && <SettingsView />}
          </div>
        </Suspense>
      </ErrorBoundary>

      <CommandPalette />
      {/* App-level, not inside a view: upgraded users with an existing index never see
          Onboarding, so the one-time analytics question has to live here. It sits
          bottom-left so it never covers the toasts (bottom-right), including the one
          that announces an update. */}
      <TelemetryConsent />
    </div>
  );
}

export default App;
