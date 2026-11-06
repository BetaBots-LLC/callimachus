import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { defaultFilter } from "cmdk";
import {
  BarChart3,
  Compass,
  Database,
  FolderGit2,
  History,
  Lightbulb,
  ListChecks,
  MessagesSquare,
  Moon,
  RefreshCw,
  Search,
  Settings,
  Sparkles,
  Sun,
} from "lucide-react";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { api } from "../lib/api";
import { formatTime } from "../lib/format";
import { useBuildSemanticIndex, useIndexRewriting, useReindex } from "../lib/queries";
import { useChecklist } from "../store/checklist";
import { useUi, type View } from "../store/ui";
import { useTheme } from "../store/theme";
import { useUpdater } from "../store/updater";

// Labels match the tab bar (App.tsx TABS); keywords keep the longer names findable.
const NAV: { id: View; label: string; icon: typeof Search; keywords?: string[] }[] = [
  { id: "search", label: "Search", icon: Search },
  { id: "chat", label: "Chat", icon: MessagesSquare },
  { id: "coach", label: "Coach", icon: Compass },
  { id: "knowledge", label: "Knowledge", icon: Lightbulb },
  { id: "ask", label: "Ask", icon: Sparkles, keywords: ["ask your history", "question"] },
  { id: "projects", label: "Projects", icon: FolderGit2, keywords: ["project memory"] },
  { id: "stats", label: "Stats", icon: BarChart3 },
  { id: "settings", label: "Settings", icon: Settings },
];

const SEARCH_VALUE = "search for typed text";
const THREAD_PREFIX = "thread:";

const ACTIONS = {
  reindex: "reindex history",
  build: "build semantic index embeddings",
  theme: "toggle theme dark light",
  updates: "check for updates upgrade version",
  checklist: "setup checklist onboarding getting started connect agents",
} as const;

/**
 * cmdk selects the first matching item in DOM order. It sorts items inside a group by
 * score but never moves groups (1.1.1 looks groups up by id in their data-value), so the
 * palette orders its groups itself (groupScore). "Search for" scores just above zero:
 * shown for any text, selected only when no view, thread or action matches; it won Enter
 * for every word ("stats", "settings", "dark") while it rendered first. Threads match
 * their title as a substring; the fuzzy default made "auth" match "Debug iOS push token".
 */
function paletteFilter(value: string, search: string, keywords?: string[]): number {
  if (value === SEARCH_VALUE) return 1e-9;
  if (value.startsWith(THREAD_PREFIX)) {
    const title = (keywords ?? []).join(" ");
    if (!title.toLowerCase().includes(search.trim().toLowerCase())) return 0;
    return defaultFilter(title, search);
  }
  return defaultFilter(value, search, keywords);
}

function groupScore(search: string, items: { value: string; keywords?: string[] }[]): number {
  return Math.max(0, ...items.map((i) => paletteFilter(i.value, search, i.keywords)));
}

/** Cmd/Ctrl-K command palette: jump between views + run the common actions. */
export function CommandPalette() {
  const open = useUi((s) => s.commandOpen);
  const setOpen = useUi((s) => s.setCommandOpen);
  const setView = useUi((s) => s.setView);
  const setQuery = useUi((s) => s.setQuery);
  const selectThread = useUi((s) => s.selectThread);
  // The palette's own text, kept so "Search for" can hand it to the Search view. Reset
  // during the render that opens it (any open path), not in an effect after, so the first
  // frame never shows the last draft; not on close, where the fading dialog would flash
  // the unfiltered list.
  const [typed, setTyped] = useState("");
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (open) setTyped("");
  }
  // Only fetched while the palette is open; recent_threads is a cheap ordered read.
  const recent = useQuery({
    queryKey: ["palette_recent_threads"],
    queryFn: () => api.recentThreads({ limit: 5 }),
    enabled: open,
    staleTime: 30_000,
  });
  const theme = useTheme((s) => s.resolved);
  const toggleTheme = useTheme((s) => s.toggle);
  // Shared with the search bar: same toasts, and a retry clears the last embed error.
  const reindex = useReindex();
  const buildIndex = useBuildSemanticIndex();
  const openSettings = useUi((s) => s.openSettings);
  const checkForUpdates = useUpdater((s) => s.check);
  const showChecklist = useChecklist((s) => s.show);

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

  // Both actions do nothing while Shrink index / Reclaim space rewrites the index.
  const rewriting = useIndexRewriting();

  const run = (fn: () => void) => {
    setOpen(false);
    fn();
  };

  const threads = (recent.data ?? []).slice(0, 5).map((t) => ({
    id: t.id,
    updatedAt: t.updatedAt,
    title: t.title || "Untitled thread",
  }));
  const recentGroup = threads.length > 0 && (
    <CommandGroup key="recent" heading="Recent threads">
      {threads.map((t) => (
        <CommandItem
          key={t.id}
          value={`${THREAD_PREFIX}${t.id}`}
          keywords={[t.title]}
          onSelect={() =>
            run(() => {
              selectThread(t.id);
              setView("search");
            })
          }
        >
          <History />
          <span className="min-w-0 flex-1 truncate">{t.title}</span>
          <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
            {formatTime(t.updatedAt)}
          </span>
        </CommandItem>
      ))}
    </CommandGroup>
  );

  const gotoGroup = (
    <CommandGroup key="go-to" heading="Go to">
      {NAV.map((n) => (
        <CommandItem
          key={n.id}
          value={n.label}
          keywords={n.keywords}
          onSelect={() => run(() => setView(n.id))}
        >
          <n.icon />
          <span>{n.label}</span>
        </CommandItem>
      ))}
    </CommandGroup>
  );
  const actionsGroup = (
    <CommandGroup key="actions" heading="Actions">
      <CommandItem
        value={ACTIONS.reindex}
        disabled={rewriting}
        onSelect={() => run(() => reindex.mutate())}
      >
        <RefreshCw />
        <span>Reindex history</span>
      </CommandItem>
      <CommandItem
        value={ACTIONS.build}
        disabled={rewriting}
        onSelect={() => run(() => buildIndex.mutate())}
      >
        <Database />
        <span>Build semantic index</span>
      </CommandItem>
      <CommandItem value={ACTIONS.theme} onSelect={() => run(toggleTheme)}>
        {theme === "dark" ? <Sun /> : <Moon />}
        <span>Toggle theme</span>
      </CommandItem>
      <CommandItem
        value={ACTIONS.updates}
        onSelect={() =>
          run(() => {
            openSettings("updates");
            void checkForUpdates({ manual: true });
          })
        }
      >
        <RefreshCw />
        <span>Check for updates</span>
      </CommandItem>
      <CommandItem value={ACTIONS.checklist} onSelect={() => run(() => showChecklist())}>
        <ListChecks />
        <span>Setup checklist</span>
      </CommandItem>
    </CommandGroup>
  );
  // Recent threads lead while nothing is typed, so they show above the fold; with text,
  // the best-scoring group comes first (see paletteFilter).
  const search = typed.trim();
  const orderedGroups = !search
    ? [recentGroup, gotoGroup, actionsGroup]
    : [
        {
          node: recentGroup,
          score: groupScore(
            search,
            threads.map((t) => ({ value: `${THREAD_PREFIX}${t.id}`, keywords: [t.title] })),
          ),
        },
        {
          node: gotoGroup,
          score: groupScore(
            search,
            NAV.map((n) => ({ value: n.label, keywords: n.keywords })),
          ),
        },
        {
          node: actionsGroup,
          score: groupScore(
            search,
            // A disabled item can't take Enter, so it can't lift its group either.
            Object.values(ACTIONS)
              .filter((v) => !(rewriting && (v === ACTIONS.reindex || v === ACTIONS.build)))
              .map((value) => ({ value })),
          ),
        },
      ]
        .sort((a, b) => b.score - a.score)
        .map((g) => g.node);

  // Same markup as ui/command's CommandDialog, which can't pass `filter` to the Command.
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogHeader className="sr-only">
        <DialogTitle>Command Palette</DialogTitle>
        <DialogDescription>
          Jump to a view, open a thread, run an action or search.
        </DialogDescription>
      </DialogHeader>
      <DialogContent
        className="top-1/3 translate-y-0 overflow-hidden rounded-xl! p-0"
        showCloseButton={false}
      >
        <Command filter={paletteFilter}>
          <CommandInput
            placeholder="Jump to a view, open a thread or search…"
            value={typed}
            onValueChange={setTyped}
          />
          <CommandList>
            {/* With text typed, the Search item below always matches, so there is no empty state. */}
            {!typed.trim() && <CommandEmpty>No matches.</CommandEmpty>}
            {orderedGroups}
            {typed.trim() && (
              // Always last and scored lowest (paletteFilter), so Enter picks it only when
              // nothing else matches. forceMount on the group too: cmdk hides a group it
              // mounted mid-filter otherwise.
              <CommandGroup forceMount>
                <CommandItem
                  forceMount
                  value={SEARCH_VALUE}
                  onSelect={() =>
                    run(() => {
                      useChecklist.getState().close();
                      setQuery(typed.trim());
                      setView("search");
                    })
                  }
                >
                  <Search />
                  <span className="truncate">Search for "{typed.trim()}"</span>
                </CommandItem>
              </CommandGroup>
            )}
          </CommandList>
        </Command>
      </DialogContent>
    </Dialog>
  );
}
