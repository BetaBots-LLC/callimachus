import { Activity, lazy, Suspense, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { BarChart3, Moon, Settings, Sun } from "lucide-react";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { SearchBar } from "./components/SearchBar";
import { ResultsList } from "./components/ResultsList";
import { KnowledgeGate } from "./components/KnowledgeGate";
import { TelemetryConsent } from "./components/TelemetryConsent";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Loading } from "./components/Loading";
import { preloadMarkdown } from "./components/LazyMarkdown";
import { Button } from "@/components/ui/button";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { api } from "./lib/api";
import { MOD_KEY } from "./lib/platform";
import { prefetchOnIntent, warm, whenIdle } from "./lib/prefetch";
import { useChecklist } from "./store/checklist";
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

// Parts of the Search view that first paint never shows also stay out of the entry: the
// thread pane (with its tag combobox and knowledge sheet) until a thread is opened, the
// command palette (cmdk plus a Radix dialog stack) until Cmd/Ctrl-K, and the setup
// checklist unless the index is empty or the user opens it.
const loadThreadView = () => import("./components/ThreadView");
const loadCommandPalette = () => import("./components/CommandPalette");
const ThreadView = lazy(() => loadThreadView().then((m) => ({ default: m.ThreadView })));
const CommandPalette = lazy(() =>
  loadCommandPalette().then((m) => ({ default: m.CommandPalette })),
);
const Onboarding = lazy(() =>
  import("./components/Onboarding").then((m) => ({ default: m.Onboarding })),
);

/** Warm the lazy view chunks once the app is idle, so the first visit to a tab doesn't
 *  flash a loading state. Runs after first paint, so startup is unaffected. */
const PREFETCH_DELAY_MS = 2_000;
function usePrefetchViews() {
  useEffect(() => {
    // The markdown stack is split out of the entry only to keep it off the splash path,
    // and opening a thread is the first thing most people do, so fetch it right after
    // first paint rather than with the other views.
    preloadMarkdown().catch(() => {}); // a real failure resurfaces on render, inside the boundary
    // When the main thread is idle, or after the delay at the latest; hovering a tab warms
    // its chunk earlier (prefetchOnIntent below).
    return whenIdle(() => {
      for (const load of [
        loadThreadView,
        loadCommandPalette,
        loadChat,
        loadSettings,
        loadStats,
        loadCoach,
        loadKnowledge,
        loadAsk,
        loadProjects,
      ]) {
        warm(load); // a real failure resurfaces on visit, inside the boundary
      }
    }, PREFETCH_DELAY_MS);
  }, []);
}

/** The open thread's cache entry, held from outside the Search tab's Activity. A hidden
 *  Activity unsubscribes its queries, and ThreadView's one-minute gcTime would then drop
 *  the transcript during a longer visit to another tab, so coming back reloaded it. This
 *  observer never fetches (enabled: false) and reads nothing, so it never re-renders. */
function RetainOpenThread() {
  const threadId = useUi((s) => s.selectedThreadId);
  useQuery({
    queryKey: ["thread", threadId],
    queryFn: () => api.getThread(threadId as number),
    enabled: false,
    gcTime: 60_000,
  });
  return null;
}

/** The thread pane: a plain placeholder until a thread is selected, so the ThreadView
 *  chunk loads on first hover over the results (or the idle prefetch), not at startup. */
function ThreadPane() {
  const threadId = useUi((s) => s.selectedThreadId);
  if (threadId == null)
    return (
      <div className="flex h-full items-center justify-center text-muted-foreground">
        Select a thread to read it.
      </div>
    );
  return (
    <Suspense fallback={<Loading label="Loading…" className="h-full" />}>
      <ThreadView />
    </Suspense>
  );
}

/** Mounts the lazy command palette on its first open and owns Cmd/Ctrl-K until the
 *  palette has rendered (its chunk can still be loading after the first press, and a
 *  second press then has to close it). From then CommandPalette's own listener takes
 *  over, so a press never toggles twice. */
function CommandPaletteHost() {
  const open = useUi((s) => s.commandOpen);
  const [mounted, setMounted] = useState(false);
  const [ready, setReady] = useState(false);
  // Set in a layout effect, before CommandPalette's passive effect adds its listener, so
  // no keypress in between reaches both listeners.
  const readyRef = useRef(false);
  useEffect(() => {
    if (open) setMounted(true);
  }, [open]);
  useEffect(() => {
    if (ready) return;
    const onKey = (e: KeyboardEvent) => {
      if (readyRef.current) return;
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        const ui = useUi.getState();
        ui.setCommandOpen(!ui.commandOpen);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [ready]);
  if (!mounted) return null;
  // Its own boundary with no fallback: a chunk that fails to load must not take the whole
  // window to the app-level crash screen. The error is still logged.
  return (
    <ErrorBoundary fallback={null}>
      <Suspense fallback={null}>
        <CommandPalette />
        <PaletteRendered
          onRendered={() => {
            readyRef.current = true;
            setReady(true);
          }}
        />
      </Suspense>
    </ErrorBoundary>
  );
}

/** Reports that the Suspense boundary around it committed, meaning its sibling (the lazy
 *  palette) has loaded and rendered. */
function PaletteRendered({ onRendered }: { onRendered: () => void }) {
  // biome-ignore lint/correctness/useExhaustiveDependencies: once, on the first commit
  useLayoutEffect(() => {
    onRendered();
  }, []);
  return null;
}

const warmThreadView = () => {
  loadThreadView().catch(() => {}); // a real failure resurfaces on open, inside the boundary
};

// Content views shown as labeled tabs. Utilities (Stats, Settings) live as right-side icons.
const TABS: { id: View; label: string; load?: () => Promise<unknown> }[] = [
  { id: "search", label: "Search" },
  { id: "chat", label: "Chat", load: loadChat },
  { id: "coach", label: "Coach", load: loadCoach },
  { id: "knowledge", label: "Knowledge", load: loadKnowledge },
  { id: "ask", label: "Ask", load: loadAsk },
  { id: "projects", label: "Projects", load: loadProjects },
];

function App() {
  usePrefetchViews();
  const view = useUi((s) => s.view);
  const setView = useUi((s) => s.setView);
  const setCommandOpen = useUi((s) => s.setCommandOpen);
  const theme = useTheme((s) => s.resolved);
  const toggleTheme = useTheme((s) => s.toggle);
  const { data: stats } = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  // An empty index opens the setup checklist, which then stays open until the user closes it
  // or opens a thread or a search (store/checklist.ts): the index step finishing must not
  // pull the other three steps away mid-setup.
  const indexEmpty = !!stats && stats.threads === 0;
  const checklistOpen = useChecklist((s) => s.open);
  const openFirstRun = useChecklist((s) => s.openFirstRun);
  useEffect(() => {
    if (indexEmpty) openFirstRun();
  }, [indexEmpty, openFirstRun]);
  // LLM distillation gates Ask only. Knowledge and Projects always render (TODOs, Remember
  // and harvested memory need no engine) and show the memory card inline while it is off.
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
                {...prefetchOnIntent(t.load)}
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

      {/* The Search tab stays mounted while another tab is open (React keeps a hidden
          Activity's state and DOM), so coming back keeps both scroll positions and the open
          thread instead of rebuilding them and refetching a multi-MB transcript. Its
          boundary resets on a tab switch (resetOn), as the keyed boundary below does by
          remounting. */}
      <Activity mode={view === "search" ? "visible" : "hidden"}>
        <ErrorBoundary scope="view" resetOn={view}>
          <Suspense fallback={<Loading label="Loading…" className="flex-1" />}>
            <div className="flex min-h-0 flex-1 flex-col">
              {indexEmpty || checklistOpen || forceOnboard ? (
                <Onboarding />
              ) : (
                <>
                  <SearchBar />
                  <main className="grid min-h-0 flex-1 grid-cols-[minmax(300px,min(420px,40%))_minmax(0,1fr)]">
                    {/* biome-ignore lint/a11y/noStaticElementInteractions: not interactive; hovering or focusing a result is the cue a thread is about to open, so it only warms the ThreadView chunk */}
                    <section
                      className="min-h-0 border-r"
                      onPointerOver={warmThreadView}
                      onFocus={warmThreadView}
                    >
                      <ResultsList />
                    </section>
                    <section className="min-h-0 min-w-0">
                      <ThreadPane />
                    </section>
                  </main>
                </>
              )}
            </div>
          </Suspense>
        </ErrorBoundary>
      </Activity>
      {/* Keyed by view: a crash in one tab shows an inline fallback there, and switching
          tabs remounts a fresh boundary so the rest of the app keeps working. */}
      {view !== "search" && (
        <ErrorBoundary key={view} scope="view">
          <Suspense fallback={<Loading label="Loading…" className="flex-1" />}>
            <div className="flex min-h-0 flex-1 flex-col">
              {view === "chat" && <ChatView />}
              {view === "coach" && <CoachView />}
              {view === "knowledge" && <KnowledgeView />}
              {view === "ask" && (
                <KnowledgeGate
                  enabled={on}
                  feature="Ask your history"
                  blurb="Ask a question and get a synthesized, cited answer drawn from your own past sessions."
                >
                  <AskView />
                </KnowledgeGate>
              )}
              {view === "projects" && <ProjectMemoryView />}
              {view === "stats" && <StatsView />}
              {view === "settings" && <SettingsView />}
            </div>
          </Suspense>
        </ErrorBoundary>
      )}

      <RetainOpenThread />
      <CommandPaletteHost />
      {/* App-level, not inside a view: upgraded users with an existing index never see the
          setup checklist unless they open it, so the one-time analytics question has to
          live here too. While the checklist is open it asks in the checklist's footer
          instead. It sits bottom-left so it never covers the toasts (bottom-right),
          including the one that announces an update. */}
      <TelemetryConsent />
    </div>
  );
}

export default App;
