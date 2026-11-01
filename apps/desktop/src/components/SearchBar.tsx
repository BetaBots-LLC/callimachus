import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useDebouncedCallback } from "@tanstack/react-pacer";
import {
  api,
  INDEXABLE_SOURCES,
  type IndexProgress,
  SOURCE_LABELS,
  type SourceKind,
} from "../lib/api";
import { useUi } from "../store/ui";
import {
  EMBED_ERROR_KEY,
  useEmbedError,
  useEmbedStatus,
  useIndexError,
  useIndexRewriting,
} from "../lib/queries";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Spinner } from "@/components/ui/spinner";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";
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
  const queryClient = useQueryClient();

  const tags = useQuery({ queryKey: ["list_tags"], queryFn: api.listTags, staleTime: 60_000 });

  // Seed from the committed query: this bar unmounts whenever another tab is shown, and
  // the results list keeps filtering by the stored query, so the box must show it again.
  const [text, setText] = useState(() => useUi.getState().query);
  // Pacer handles the debounce (no manual timer / useEffect).
  const commitQuery = useDebouncedCallback((value: string) => setQuery(value.trim()), {
    wait: 260,
  });

  function onChange(value: string) {
    setText(value);
    commitQuery(value);
  }

  function clear() {
    setText("");
    // Supersede any pending debounced value (it would re-apply the old text), and clear
    // the committed query right away instead of after the debounce.
    commitQuery("");
    setQuery("");
  }

  // Reindex runs in the BACKGROUND now: the button just kicks it off and reflects the
  // job flag, so it never blocks the UI. main.tsx refreshes results/stats on index:done.
  const indexing = useQuery({
    queryKey: ["index_status"],
    queryFn: api.indexingStatus,
    refetchInterval: (q) => (q.state.data ? 1500 : false),
  });
  const reindex = useMutation({
    mutationFn: api.indexAll,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["index_status"] }),
  });

  // Event-driven; never polls while idle (see lib/queries.ts).
  const embed = useEmbedStatus();
  const embedError = useEmbedError();
  // A failed full reindex otherwise ends with no visible change after clicking Reindex.
  const indexError = useIndexError();
  // Reindex and Build return without doing anything while Shrink index / Reclaim space runs.
  const rewriting = useIndexRewriting();
  const buildIndex = useMutation({
    mutationFn: api.buildEmbeddings,
    // A retry starts clean; a new failure arrives as a fresh embed:error.
    onMutate: () => queryClient.setQueryData(EMBED_ERROR_KEY, null),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["embed_status"] }),
  });

  const embedPct =
    embed.data && embed.data.total > 0 ? Math.round((embed.data.done / embed.data.total) * 100) : 0;

  // Per-source reindex progress, pushed via the index:progress event (see main.tsx).
  const indexProgress = useQuery<IndexProgress | null>({
    queryKey: ["index_progress"],
    queryFn: () => queryClient.getQueryData<IndexProgress>(["index_progress"]) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });

  // Background auto-distill: status flag + per-thread progress (distill:* events).
  const distilling = useQuery({
    queryKey: ["distill_status"],
    queryFn: api.distillingStatus,
    refetchInterval: (q) => (q.state.data ? 1500 : false),
  });
  const distillProgress = useQuery<{ done: number; total: number } | null>({
    queryKey: ["distill_progress"],
    queryFn: () =>
      queryClient.getQueryData<{ done: number; total: number }>(["distill_progress"]) ?? null,
    staleTime: Number.POSITIVE_INFINITY,
  });

  // One progress bar, whichever long job is active. Priority: index > embed > distill
  // (distill is background and yields to the others).
  const ip = indexProgress.data;
  const dp = distillProgress.data;
  const jobActive = embed.data?.running || indexing.data || distilling.data;
  const jobPct = embed.data?.running
    ? embedPct
    : indexing.data
      ? ip && ip.total > 0
        ? Math.round((ip.done / ip.total) * 100)
        : 0
      : dp && dp.total > 0
        ? Math.round((dp.done / dp.total) * 100)
        : 0;
  const jobLabel = embed.data?.running
    ? `Building semantic index · ${embed.data.done.toLocaleString()} / ${embed.data.total.toLocaleString()} messages`
    : indexing.data
      ? ip?.current
        ? `Indexing ${SOURCE_LABELS[ip.current as SourceKind] ?? ip.current}…`
        : "Indexing…"
      : dp
        ? `Distilling knowledge · ${dp.done} / ${dp.total} threads`
        : "Distilling knowledge…";

  // How many "More" sources are active — surfaced on the dropdown so a hidden
  // filter is never silently in effect.
  const moreActiveCount = MORE.filter((s) => sources.includes(s)).length;

  return (
    <div className="flex flex-col gap-2 border-b px-4 py-3">
      <div className="flex gap-2">
        <InputGroup>
          <InputGroupInput
            placeholder="Search every thread…  (try file:embed/mod.rs)"
            value={text}
            autoFocus
            onChange={(e) => onChange(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape" && text) {
                e.preventDefault();
                clear();
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
          variant="secondary"
          onClick={() => reindex.mutate()}
          disabled={indexing.data || reindex.isPending || embed.data?.running || rewriting}
        >
          {indexing.data ? (
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
          {indexError && !indexing.data && (
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
              disabled={indexing.data || buildIndex.isPending || rewriting}
              title={`${embed.data.done}/${embed.data.total} messages embedded`}
            >
              Build semantic index ({embedPct}%)
            </Button>
          ) : embed.data && embed.data.total > 0 ? (
            <span>semantic ready</span>
          ) : null}
        </span>
      </div>

      {jobActive &&
        (indexing.data ? (
          // Reindex total is estimated from the existing thread count (accurate on a
          // re-index, indeterminate on a first run). The live "N scanned" count keeps a
          // long source (e.g. Claude Code) from ever looking stuck.
          <Progress
            value={ip && ip.total > 0 ? Math.min(99, Math.round((ip.done / ip.total) * 100)) : null}
            className="gap-1.5"
          >
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              {jobLabel}
            </ProgressLabel>
            <span className="ml-auto text-xs text-muted-foreground tabular-nums">
              {(ip?.done ?? 0).toLocaleString()} scanned
            </span>
          </Progress>
        ) : (
          <Progress value={jobPct} className="gap-1.5">
            <ProgressLabel className="text-xs font-normal text-muted-foreground">
              {jobLabel}
            </ProgressLabel>
            <ProgressValue className="text-xs" />
          </Progress>
        ))}
    </div>
  );
}

function Separator() {
  return <span className="mx-1 h-4 w-px bg-border" />;
}
