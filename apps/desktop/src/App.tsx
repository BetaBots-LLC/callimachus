import { useQuery } from "@tanstack/react-query";
import { Moon, Sun } from "lucide-react";
import { SearchBar } from "./components/SearchBar";
import { ResultsList } from "./components/ResultsList";
import { ThreadView } from "./components/ThreadView";
import { ChatView } from "./components/ChatView";
import { TodosView } from "./components/TodosView";
import { StatsView } from "./components/StatsView";
import { SettingsView } from "./components/SettingsView";
import { Button } from "@/components/ui/button";
import { api } from "./lib/api";
import { useUi, type View } from "./store/ui";
import { useTheme } from "./store/theme";

const TABS: { id: View; label: string }[] = [
  { id: "search", label: "Search" },
  { id: "chat", label: "Chat" },
  { id: "stats", label: "Stats" },
  { id: "settings", label: "Settings" },
];

function App() {
  const view = useUi((s) => s.view);
  const setView = useUi((s) => s.setView);
  const theme = useTheme((s) => s.theme);
  const toggleTheme = useTheme((s) => s.toggle);
  const { data: stats } = useQuery({ queryKey: ["db_stats"], queryFn: api.dbStats });
  // The knowledge feature is opt-in; only surface the Todos tab once it's enabled.
  const knowledge = useQuery({ queryKey: ["knowledge_config"], queryFn: api.knowledgeConfig });
  const tabs: { id: View; label: string }[] = [
    ...TABS.slice(0, 2),
    ...(knowledge.data?.enabled ? [{ id: "todos" as View, label: "Todos" }] : []),
    ...TABS.slice(2),
  ];

  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <header className="flex items-center justify-between border-b px-4 py-2">
        <div className="flex items-center gap-3">
          <span className="font-semibold tracking-tight">Callimachus</span>
          <nav className="flex gap-1">
            {tabs.map((t) => (
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
        <div className="flex items-center gap-3">
          {stats && (
            <span className="text-xs text-muted-foreground">
              {stats.threads.toLocaleString()} threads · {stats.messages.toLocaleString()} messages
            </span>
          )}
          <Button size="icon" variant="ghost" onClick={toggleTheme} aria-label="Toggle theme">
            {theme === "dark" ? <Sun /> : <Moon />}
          </Button>
        </div>
      </header>

      <div className="flex min-h-0 flex-1 flex-col">
        {view === "search" && (
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
        )}
        {view === "chat" && <ChatView />}
        {view === "todos" && <TodosView />}
        {view === "stats" && <StatsView />}
        {view === "settings" && <SettingsView />}
      </div>
    </div>
  );
}

export default App;
