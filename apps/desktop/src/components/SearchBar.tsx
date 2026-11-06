import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import {
  api,
  type EmbedStatus,
  INDEXABLE_SOURCES,
  SOURCE_LABELS,
  type SourceKind,
} from "../lib/api";
import { RESULTS_LIST_ID, resultOptionId, resultsNav, SEARCH_INPUT_ID, useUi } from "../store/ui";
import {
  embedStatusQuery,
  useBuildSemanticIndex,
  useBuildStarting,
  useEmbedError,
  useIndexError,
  useIndexRewriting,
  useJobsStatus,
  useReindex,
  useReindexStarting,
  userReindexing,
} from "../lib/queries";
import { CatchUpNote, JobProgress } from "./JobProgress";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Spinner } from "@/components/ui/spinner";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Check, ChevronDown, Star, X } from "lucide-react";
import { cn } from "@/lib/utils";

// The three most-used sources get top-level chips; the rest live under "More".
const PRIMARY: SourceKind[] = ["claude_code", "codex", "cursor"];
const MORE: SourceKind[] = [...INDEXABLE_SOURCES.filter((s) => !PRIMARY.includes(s)), "in_app"];

// While a build runs this bar only needs `running` (the counts are JobProgress's), so the
// running state collapses to one constant and structural sharing skips the re-render.
const EMBED_RUNNING: EmbedStatus = { done: 0, total: 0, running: true };
function embedSummary(s: EmbedStatus): EmbedStatus {
  return s.running ? EMBED_RUNNING : s;
}

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
  const indexEmpty = useUi((s) => s.indexEmpty);

  const tags = useQuery({ queryKey: ["list_tags"], queryFn: api.listTags, staleTime: 60_000 });

  // Seed from the committed query: this bar remounts after Onboarding gives way to the
  // index, and the results list keeps filtering by the stored query, so the box must show
  // it again.
  const [text, setText] = useState(() => useUi.getState().query);
  // The committed query can also change from outside this bar (a Coach error row, the
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

  // Reindex runs in the BACKGROUND now: the button just kicks it off and reflects the
  // job flag, so it never blocks the UI. main.tsx refreshes results/stats on index:done.
  // Only a user-started reindex counts: the launch catch-up scan is its own job and stays
  // quiet (CatchUpNote), so it no longer flashes "Indexing…" on every launch.
  const indexing = userReindexing(useJobsStatus().data);
  const reindex = useReindex();
  const reindexStarting = useReindexStarting();

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
                } else {
                  resultsNav.open(
                    highlightedIndex >= 0 && highlightedIndex < count ? highlightedIndex : 0,
                  );
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
        <Button
          // On an empty index the empty state's Reindex is the one primary action.
          variant={indexEmpty ? "outline" : "secondary"}
          onClick={() => reindex.mutate()}
          disabled={indexing || reindexStarting || embed.data?.running || rewriting}
        >
          {indexing ? (
            <>
              <Spinner /> Indexing…
            </>
          ) : (
            "Reindex"
          )}
        </Button>
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

function Separator() {
  return <span className="mx-1 h-4 w-px bg-border" />;
}
