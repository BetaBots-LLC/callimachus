import { lazy, Suspense, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import {
  api,
  type EmbedStatus,
  INDEXABLE_SOURCES,
  SOURCE_LABELS,
  type SourceKind,
} from "../lib/api";
import {
  type DatePreset,
  RESULTS_LIST_ID,
  resultOptionId,
  resultsNav,
  SEARCH_INPUT_ID,
  useUi,
} from "../store/ui";
import { baseName, rangeLabel } from "../lib/format";
import {
  embedStatusQuery,
  useBuildSemanticIndex,
  useBuildStarting,
  useEmbedError,
  useIndexError,
  useIndexRewriting,
  useJobsStatus,
  userReindexing,
} from "../lib/queries";
import { AGGREGATE_STALE_MS } from "../lib/queryClient";
import { CatchUpNote, JobProgress } from "./JobProgress";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Check, ChevronDown, FolderGit2, Star, X } from "lucide-react";
import { cn } from "@/lib/utils";

// The repo picker is a Base UI combobox, which the entry bundle otherwise leaves to the
// thread pane's lazy chunk (App.tsx); it loads right after first paint instead.
const ProjectScope = lazy(() =>
  import("./ProjectScope").then((m) => ({ default: m.ProjectScope })),
);

// The three most-used sources get top-level chips; the rest live under "More".
const PRIMARY: SourceKind[] = ["claude_code", "codex", "cursor"];
const MORE: SourceKind[] = [...INDEXABLE_SOURCES.filter((s) => !PRIMARY.includes(s)), "in_app"];

// While a build runs this bar only needs `running` (the counts are JobProgress's), so the
// running state collapses to one constant and structural sharing skips the re-render.
const EMBED_RUNNING: EmbedStatus = { done: 0, total: 0, running: true };
function embedSummary(s: EmbedStatus): EmbedStatus {
  return s.running ? EMBED_RUNNING : s;
}

// Semantic search turns itself on once this share of messages is embedded: below it, a
// hybrid query mostly fuses keyword hits with a partial vector index, and the build
// button in this bar is the better nudge.
const AUTO_HYBRID_COVERAGE = 0.9;

// The model loads on the first hybrid query of a session, not when the switch turns
// itself on at launch: loading ONNX for a window that never searches would undo the
// idle-memory work. Warmed once per session on the first keystroke instead.
let semanticWarmed = false;

export function SearchBar() {
  const setQuery = useUi((s) => s.setQuery);
  const sources = useUi((s) => s.sources);
  const toggleSource = useUi((s) => s.toggleSource);
  const includeSubagents = useUi((s) => s.includeSubagents);
  const toggleSubagents = useUi((s) => s.toggleSubagents);
  const hybrid = useUi((s) => s.hybrid);
  const toggleHybrid = useUi((s) => s.toggleHybrid);
  const starredOnly = useUi((s) => s.starredOnly);
  const toggleStarredOnly = useUi((s) => s.toggleStarredOnly);
  const selectedTags = useUi((s) => s.selectedTags);
  const toggleTag = useUi((s) => s.toggleTag);
  const highlightedIndex = useUi((s) => s.highlightedIndex);
  const setHighlightedIndex = useUi((s) => s.setHighlightedIndex);
  const resultCount = useUi((s) => s.resultCount);
  const setOpenFirstPending = useUi((s) => s.setOpenFirstPending);

  const tags = useQuery({ queryKey: ["list_tags"], queryFn: api.listTags, staleTime: 60_000 });

  // The scope defaults to the repo the Project page last showed, unless the user picked or
  // cleared one this session. An older backend without the command rejects: no default.
  const scopeReady = useUi((s) => s.scopeReady);
  const applyDefaultProject = useUi((s) => s.applyDefaultProject);
  const lastProject = useQuery({
    queryKey: ["last_project"],
    queryFn: api.lastProject,
    staleTime: Number.POSITIVE_INFINITY,
    retry: false,
    enabled: !scopeReady,
  });
  // Applied only while it is still a listed repo, as the Project page checks it: a renamed,
  // deleted or re-keyed repo (or a worktree path) would otherwise scope every launch to
  // "Nothing in X". Same key as ProjectScope and the Project page, so it is fetched once.
  // "*" is the Project page's "All projects" sentinel, which is no scope for Search.
  const remembered = lastProject.data && lastProject.data !== "*" ? lastProject.data : null;
  const projects = useQuery({
    queryKey: ["projects"],
    queryFn: api.listProjects,
    staleTime: AGGREGATE_STALE_MS,
    enabled: !scopeReady && remembered !== null,
  });
  useEffect(() => {
    if (scopeReady || lastProject.isPending) return;
    if (remembered === null) return applyDefaultProject(null);
    if (projects.isPending) return;
    applyDefaultProject(projects.data?.some((p) => p.project === remembered) ? remembered : null);
  }, [
    scopeReady,
    lastProject.isPending,
    remembered,
    projects.isPending,
    projects.data,
    applyDefaultProject,
  ]);

  // Seed from the committed query: this bar remounts after Onboarding gives way to the
  // index, and the results list keeps filtering by the stored query, so the box must show
  // it again.
  const [text, setText] = useState(() => useUi.getState().query);
  // The committed query can also change from outside this bar (a Stats heatmap day, the
  // palette's "Search for"), and the bar now stays mounted, so mirror those into the box.
  // Its own commits are debounced, so only a value this bar did not just commit counts.
  const query = useUi((s) => s.query);
  const committedHere = useRef(query);
  useEffect(() => {
    if (query !== committedHere.current) {
      committedHere.current = query;
      setText(query);
    }
  }, [query]);
  // Focus the box on mount and each time the Search tab is shown again. autoFocus fires
  // only on DOM insertion, and the tab now stays mounted (a hidden Activity) while another
  // tab is open; layout effects re-run on reveal.
  const inputRef = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => {
    inputRef.current?.focus();
  }, []);
  // Pacer handles the debounce (no manual timer / useEffect).
  const commitQuery = useDebouncedCallback(
    (value: string) => {
      committedHere.current = value.trim();
      setQuery(value.trim());
    },
    { wait: 260 },
  );

  function onChange(value: string) {
    setText(value);
    if (!semanticWarmed && useUi.getState().hybrid) {
      semanticWarmed = true;
      api.warmSemantic().catch(() => {}); // an older backend without it just rejects
    }
    setOpenFirstPending(false); // an Enter pressed for the shorter text no longer applies
    commitQuery(value);
  }

  // The keys act on the rows of the text in the box. While the debounce still holds it
  // back, commit it now and report that the rows on screen are not its rows yet (ResultsList
  // also publishes 0 rows while it shows the previous query's as placeholder). The pending
  // debounced call then commits the same value again, which setQuery ignores.
  function commitTextNow(): boolean {
    const value = text.trim();
    if (value === useUi.getState().query) return false;
    committedHere.current = value;
    setQuery(value);
    return true;
  }

  function clear() {
    setText("");
    setOpenFirstPending(false);
    // Supersede any pending debounced value (it would re-apply the old text), and clear
    // the committed query right away instead of after the debounce.
    commitQuery("");
    committedHere.current = "";
    setQuery("");
  }

  // A user-started reindex (palette, Settings) runs in the background; while it does, the
  // build button waits and a failure shows here. The launch catch-up scan is its own job and
  // stays quiet (CatchUpNote).
  const indexing = userReindexing(useJobsStatus().data);

  // Event-driven; never polls while idle (see lib/queries.ts). Selected down to what this
  // bar shows, so a running build's per-batch embed:progress (which only JobProgress
  // draws) does not re-render the input and chips.
  const embed = useQuery({ ...embedStatusQuery, select: embedSummary });
  const embedError = useEmbedError();
  // A failed full reindex otherwise ends with no visible change after clicking Reindex.
  const indexError = useIndexError();
  // Reindex and Build return without doing anything while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();
  const buildIndex = useBuildSemanticIndex();
  const buildStarting = useBuildStarting();

  const embedPct =
    embed.data && embed.data.total > 0 ? Math.round((embed.data.done / embed.data.total) * 100) : 0;
  // Hybrid by default once the semantic index covers enough; the switch still turns it off
  // (and from then on this session keeps the user's choice).
  const autoHybrid = useUi((s) => s.autoHybrid);
  const coverage = embed.data && embed.data.total > 0 ? embed.data.done / embed.data.total : 0;
  useEffect(() => {
    if (coverage >= AUTO_HYBRID_COVERAGE) autoHybrid();
  }, [coverage, autoHybrid]);

  // How many "More" sources are active, surfaced on the dropdown so a hidden
  // filter is never silently in effect.
  const moreActiveCount = MORE.filter((s) => sources.includes(s)).length;

  return (
    <div className="flex flex-col gap-2 border-b px-4 py-3">
      <div className="flex gap-2">
        <InputGroup>
          <InputGroupInput
            placeholder="Search every thread…  (try file:embed/mod.rs)"
            ref={inputRef}
            id={SEARCH_INPUT_ID}
            value={text}
            onChange={(e) => onChange(e.currentTarget.value)}
            // A combobox over the results listbox: focus stays here while ArrowUp/ArrowDown
            // move the highlighted row and Enter opens it.
            role="combobox"
            aria-label="Search threads"
            aria-autocomplete="list"
            aria-controls={RESULTS_LIST_ID}
            aria-expanded={resultCount > 0}
            aria-activedescendant={
              highlightedIndex >= 0 && highlightedIndex < resultCount
                ? resultOptionId(highlightedIndex)
                : undefined
            }
            onKeyDown={(e) => {
              if (e.key === "Escape" && text) {
                e.preventDefault();
                clear();
                return;
              }
              if (e.nativeEvent.isComposing) return;
              const count = resultCount;
              if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                // Uncommitted text: commit it and drop the key until its rows arrive.
                if (commitTextNow()) {
                  e.preventDefault();
                  return;
                }
                if (count === 0) return;
                e.preventDefault(); // keep the caret where it is
                const step = e.key === "ArrowDown" ? 1 : -1;
                const from = highlightedIndex < 0 && step < 0 ? count : highlightedIndex;
                setHighlightedIndex(Math.min(count - 1, Math.max(0, from + step)));
              } else if (e.key === "Enter") {
                e.preventDefault();
                // Enter opens the highlighted row, or the top one. Before the rows of the
                // typed text exist, ResultsList opens the top row once they arrive.
                if (commitTextNow() || count === 0) {
                  setOpenFirstPending(true);
                } else if (highlightedIndex >= 0 && highlightedIndex < count) {
                  resultsNav.open(highlightedIndex);
                } else {
                  resultsNav.openTop();
                }
              }
            }}
          />
          {text && (
            <InputGroupAddon align="inline-end">
              <InputGroupButton size="icon-xs" aria-label="Clear search" onClick={clear}>
                <X />
              </InputGroupButton>
            </InputGroupAddon>
          )}
        </InputGroup>
        <Suspense fallback={<ProjectScopeFallback />}>
          <ProjectScope />
        </Suspense>
        <DateScope />
        {/* No Reindex button here: the watcher indexes new sessions as they land, so a full
            reindex is a repair, kept in the command palette (Settings > Sources reindexes
            one source, and the empty index has its own button). A running one still shows
            in JobProgress. */}
      </div>

      <div className="flex flex-wrap items-center gap-2">
        {PRIMARY.map((s) => (
          <Button
            key={s}
            size="xs"
            variant={sources.includes(s) ? "default" : "outline"}
            onClick={() => toggleSource(s)}
            title={sources.length === 0 ? "All sources" : undefined}
          >
            {SOURCE_LABELS[s]}
          </Button>
        ))}

        <DropdownMenu>
          <DropdownMenuTrigger
            render={<Button size="xs" variant={moreActiveCount > 0 ? "default" : "outline"} />}
          >
            More{moreActiveCount > 0 ? ` (${moreActiveCount})` : ""}
            <ChevronDown className="size-3.5" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start">
            {MORE.map((s) => (
              <DropdownMenuItem key={s} closeOnClick={false} onClick={() => toggleSource(s)}>
                <Check
                  className={cn("size-4", sources.includes(s) ? "opacity-100" : "opacity-0")}
                />
                {SOURCE_LABELS[s]}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>

        <Separator />
        <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Switch checked={includeSubagents} onCheckedChange={toggleSubagents} />
          subagents
        </label>
        <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Switch checked={hybrid} onCheckedChange={toggleHybrid} />
          semantic
        </label>

        <Separator />
        <Button
          size="xs"
          variant={starredOnly ? "default" : "outline"}
          onClick={toggleStarredOnly}
          title="Only starred threads"
        >
          <Star className={cn("size-3.5", starredOnly && "fill-current")} />
          Starred
        </Button>
        {tags.data?.map(([tag, count]) => (
          <Button
            key={tag}
            size="xs"
            variant={selectedTags.includes(tag) ? "default" : "outline"}
            onClick={() => toggleTag(tag)}
            title={`${count} thread${count === 1 ? "" : "s"}`}
          >
            #{tag}
          </Button>
        ))}

        <span className="ml-auto flex items-center gap-2 text-xs text-muted-foreground">
          <CatchUpNote />
          {indexError && !indexing && (
            <span
              role="status"
              className="max-w-[32ch] truncate text-destructive"
              title={indexError}
            >
              Reindex failed: {indexError}
            </span>
          )}
          {embedError && !embed.data?.running && (
            // Short in the toolbar; the full reason is in the tooltip and in Settings.
            <span
              role="status"
              className="max-w-[32ch] truncate text-destructive"
              title={embedError}
            >
              Semantic index stopped: {embedError}
            </span>
          )}
          {embed.data?.running ? null : embed.data && embed.data.done < embed.data.total ? (
            <Button
              size="xs"
              variant="outline"
              onClick={() => buildIndex.mutate()}
              disabled={indexing || buildStarting || rewriting}
              title={`${embed.data.done}/${embed.data.total} messages embedded`}
            >
              Build semantic index ({embedPct}%)
            </Button>
          ) : embed.data && embed.data.total > 0 ? (
            <span>semantic ready</span>
          ) : null}
        </span>
      </div>

      <JobProgress />
    </div>
  );
}

/** Stands in for the repo picker while its chunk loads (right after first paint): the same
 *  button, inert for those few milliseconds. */
function ProjectScopeFallback() {
  const project = useUi((s) => s.project);
  return (
    <div className="flex shrink-0 items-center" aria-hidden>
      <Button
        variant={project ? "secondary" : "outline"}
        className={cn("max-w-48", project && "rounded-r-none")}
        tabIndex={-1}
      >
        <FolderGit2 />
        <span className="truncate">{project ? baseName(project) : "All repos"}</span>
        <ChevronDown className="size-3.5 opacity-60" />
      </Button>
      {project && (
        <Button
          variant="secondary"
          size="icon"
          className="rounded-l-none border-l border-background/60"
          tabIndex={-1}
        >
          <X />
        </Button>
      )}
    </div>
  );
}

const PRESETS: { id: Exclude<DatePreset, "custom" | "all">; label: string; title: string }[] = [
  { id: "24h", label: "24h", title: "Active in the last 24 hours" },
  { id: "7d", label: "7d", title: "Active in the last 7 days" },
  { id: "30d", label: "30d", title: "Active in the last 30 days" },
];

/** Date presets. A range set from elsewhere (a Stats heatmap day) shows as its own
 *  pressed chip, and clicking it goes back to any time. */
function DateScope() {
  const preset = useUi((s) => s.datePreset);
  const after = useUi((s) => s.after);
  const before = useUi((s) => s.before);
  const setDatePreset = useUi((s) => s.setDatePreset);
  return (
    <fieldset
      aria-label="Date range"
      className="m-0 flex h-8 min-w-0 shrink-0 items-center gap-0.5 rounded-lg border p-[3px]"
    >
      {preset === "custom" && (
        // Not a toggle: activating it clears the range, so its name says that.
        <Button
          size="xs"
          variant="secondary"
          onClick={() => setDatePreset("all")}
          aria-label={`Clear date range ${rangeLabel(after, before)}`}
          title="Clear the date range"
        >
          {rangeLabel(after, before)}
          <X aria-hidden />
        </Button>
      )}
      {PRESETS.map((p) => (
        <Button
          key={p.id}
          size="xs"
          variant={preset === p.id ? "secondary" : "ghost"}
          aria-pressed={preset === p.id}
          title={p.title}
          className={cn("tabular-nums", preset !== p.id && "text-muted-foreground")}
          onClick={() => setDatePreset(preset === p.id ? "all" : p.id)}
        >
          {p.label}
        </Button>
      ))}
      <Button
        size="xs"
        variant={preset === "all" ? "secondary" : "ghost"}
        aria-pressed={preset === "all"}
        title="Any time"
        className={cn(preset !== "all" && "text-muted-foreground")}
        onClick={() => setDatePreset("all")}
      >
        All
      </Button>
    </fieldset>
  );
}

function Separator() {
  return <span className="mx-1 h-4 w-px bg-border" />;
}
