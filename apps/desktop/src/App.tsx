import { useQuery } from "@tanstack/react-query";
import { BarChart3, Moon, Settings, Sun } from "lucide-react";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { SearchBar } from "./components/SearchBar";
import { ResultsList } from "./components/ResultsList";
import { ThreadView } from "./components/ThreadView";
import { ChatView } from "./components/ChatView";
import { CoachView } from "./components/CoachView";
import { KnowledgeView } from "./components/KnowledgeView";
import { AskView } from "./components/AskView";
import { ProjectMemoryView } from "./components/ProjectMemoryView";
import { KnowledgeGate } from "./components/KnowledgeGate";
import { Onboarding } from "./components/Onboarding";
import { CommandPalette } from "./components/CommandPalette";
import { UpdateBanner } from "./components/UpdateBanner";
import { StatsView } from "./components/StatsView";
import { SettingsView } from "./components/SettingsView";
import { Button } from "@/components/ui/button";
import { api } from "./lib/api";
import { useUi, type View } from "./store/ui";
import { useTheme } from "./store/theme";

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
  const view = useUi((s) => s.view);
  const setView = useUi((s) => s.setView);
  const setCommandOpen = useUi((s) => s.setCommandOpen);
  const theme = useTheme((s) => s.theme);
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
            className="mr-1 hidden cursor-pointer items-center gap-1 rounded-md border px-2 py-1 text-[0.7rem] text-muted-foreground transition-colors hover:bg-muted hover:text-foreground sm:flex"
          >
            <kbd className="font-sans">⌘K</kbd>
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

      <CommandPalette />
      <UpdateBanner />
    </div>
  );
}

export default App;
