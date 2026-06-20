import { useEffect } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  BarChart3,
  Database,
  FolderGit2,
  Lightbulb,
  MessagesSquare,
  Moon,
  RefreshCw,
  Search,
  Settings,
  Sparkles,
  Sun,
} from "lucide-react";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { api } from "../lib/api";
import { useUi, type View } from "../store/ui";
import { useTheme } from "../store/theme";

const NAV: { id: View; label: string; icon: typeof Search }[] = [
  { id: "search", label: "Search", icon: Search },
  { id: "chat", label: "Chat", icon: MessagesSquare },
  { id: "knowledge", label: "Knowledge", icon: Lightbulb },
  { id: "ask", label: "Ask your history", icon: Sparkles },
  { id: "projects", label: "Project Memory", icon: FolderGit2 },
  { id: "stats", label: "Stats", icon: BarChart3 },
  { id: "settings", label: "Settings", icon: Settings },
];

/** Cmd/Ctrl-K command palette: jump between views + run the common actions. */
export function CommandPalette() {
  const open = useUi((s) => s.commandOpen);
  const setOpen = useUi((s) => s.setCommandOpen);
  const setView = useUi((s) => s.setView);
  const theme = useTheme((s) => s.theme);
  const toggleTheme = useTheme((s) => s.toggle);
  const qc = useQueryClient();
  const reindex = useMutation({
    mutationFn: api.indexAll,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["index_status"] }),
  });
  const buildIndex = useMutation({ mutationFn: api.buildEmbeddings });

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen(!open);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, setOpen]);

  const run = (fn: () => void) => {
    setOpen(false);
    fn();
  };

  return (
    <CommandDialog open={open} onOpenChange={setOpen}>
      <CommandInput placeholder="Jump to a view or run a command…" />
      <CommandList>
        <CommandEmpty>No matches.</CommandEmpty>
        <CommandGroup heading="Go to">
          {NAV.map((n) => (
            <CommandItem key={n.id} value={n.label} onSelect={() => run(() => setView(n.id))}>
              <n.icon />
              <span>{n.label}</span>
            </CommandItem>
          ))}
        </CommandGroup>
        <CommandGroup heading="Actions">
          <CommandItem value="reindex history" onSelect={() => run(() => reindex.mutate())}>
            <RefreshCw />
            <span>Reindex history</span>
          </CommandItem>
          <CommandItem
            value="build semantic index embeddings"
            onSelect={() => run(() => buildIndex.mutate())}
          >
            <Database />
            <span>Build semantic index</span>
          </CommandItem>
          <CommandItem value="toggle theme dark light" onSelect={() => run(toggleTheme)}>
            {theme === "dark" ? <Sun /> : <Moon />}
            <span>Toggle theme</span>
          </CommandItem>
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
