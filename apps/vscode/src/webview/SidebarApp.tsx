// The Activity Bar sidebar: a live search box over the whole AI-history index,
// falling back to a recent-threads list, with a corpus-stats footer. Composes
// the desktop app's shadcn primitives for visual parity; all data comes from the
// host RPC bridge.

import { type ReactNode, useCallback, useEffect, useState } from "react";
import { CornerDownLeft, Copy, Search, X } from "lucide-react";
import { Button } from "@desktop/components/ui/button";
import { Input } from "@desktop/components/ui/input";
import { Badge } from "@desktop/components/ui/badge";
import { baseName, formatTime, renderSnippet, shortPath } from "@desktop/lib/format";
import type {
  InitPayload,
  SearchHit,
  Stats,
  ThreadSummary,
  WhyCommit,
  WhyState,
  WhyThread,
} from "../protocol";
import { sourceLabel } from "../protocol";
import {
  action,
  closeWhy,
  getState,
  onRefresh,
  onRelated,
  onWhy,
  request,
  setState,
} from "./bridge";

type Scope = "all" | "project";
interface Persisted {
  query?: string;
  scope?: Scope;
}

function SectionLabel({ children }: { children: ReactNode }) {
  return (
    <p className="px-2 py-1 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
      {children}
    </p>
  );
}

function Row({
  id,
  source,
  title,
  meta,
  snippetHtml,
}: {
  id: number;
  source: string;
  title: string | null;
  meta: string;
  snippetHtml?: string;
}) {
  return (
    <div className="group relative">
      <button
        type="button"
        onClick={() => action("openThread", id, title)}
        className="w-full rounded-md px-2 py-2 text-left transition-colors hover:bg-accent focus-visible:bg-accent focus-visible:outline-none"
      >
        <div className="truncate pr-12 text-sm font-medium">{title?.trim() || "(untitled)"}</div>
        {snippetHtml ? (
          <div
            className="mt-0.5 line-clamp-2 text-xs text-muted-foreground"
            // Pre-escaped by renderSnippet; only the <mark> sentinels become tags.
            dangerouslySetInnerHTML={{ __html: snippetHtml }}
          />
        ) : null}
        <div className="mt-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <Badge variant="outline" className="h-4 px-1.5 py-0 text-[10px] font-normal">
            {sourceLabel(source)}
          </Badge>
          <span className="truncate">{meta}</span>
        </div>
      </button>
      <div className="absolute right-1 top-1.5 hidden gap-0.5 group-hover:flex">
        <Button
          size="icon-xs"
          variant="ghost"
          title="Insert transcript into the active editor"
          onClick={() => action("insertThread", id)}
        >
          <CornerDownLeft />
        </Button>
        <Button
          size="icon-xs"
          variant="ghost"
          title="Copy thread context to clipboard"
          onClick={() => action("copyThread", id)}
        >
          <Copy />
        </Button>
      </div>
    </div>
  );
}

/** What a session did to the file, for its row's meta line. */
function touchLabel(t: WhyThread): string {
  switch (t.touch) {
    case "write":
    case "edit":
      return t.edits === 1 ? "edited the file once" : `edited the file ${t.edits} times`;
    case "read":
      return "read the file";
    case "mention":
      return "mentioned the file";
    default:
      return `shares ${t.overlap} ${t.overlap === 1 ? "file" : "files"} with the commit`;
  }
}

function WhyCommitBlock({ commit }: { commit: WhyCommit }) {
  return (
    <div className="pb-1">
      <p className="px-2 pt-1.5 text-xs" title={commit.sha}>
        <span className="font-mono text-muted-foreground">{commit.shortSha.slice(0, 8)}</span>{" "}
        <span className="font-medium">{commit.subject}</span>
        <span className="text-muted-foreground"> · {formatTime(commit.committedAt)}</span>
      </p>
      {commit.threads.length === 0 ? (
        <p className="px-2 py-1 text-xs text-muted-foreground">No linked session.</p>
      ) : (
        commit.threads.map((t) => (
          <div key={`${commit.sha}-${t.id}`}>
            <Row
              id={t.id}
              source={t.source}
              title={t.title}
              meta={[touchLabel(t), formatTime(t.lastActive)].filter(Boolean).join(" · ")}
            />
            {t.facts.length > 0 ? (
              <ul className="mb-1 space-y-1 pl-4 pr-2 text-xs">
                {t.facts.map((f) => (
                  <li key={f.id} className="flex gap-1.5">
                    <Badge
                      variant="outline"
                      className="h-4 shrink-0 px-1.5 py-0 text-[10px] font-normal"
                    >
                      {f.kind}
                    </Badge>
                    <span>
                      {f.text}
                      {f.rationale ? (
                        <span className="text-muted-foreground"> Because: {f.rationale}</span>
                      ) : null}
                    </span>
                  </li>
                ))}
              </ul>
            ) : null}
          </div>
        ))
      )}
    </div>
  );
}

/** "Why this line": the commits git blame names for the line and the sessions behind them. */
function WhySection({ state, onClose }: { state: WhyState; onClose: () => void }) {
  const where = `${baseName(state.file)}:${state.line}`;
  return (
    <section className="mx-1 mb-2 rounded-md border border-border pb-1">
      <div className="flex items-center justify-between pr-1">
        <SectionLabel>
          Why · <span className="font-mono normal-case">{where}</span>
        </SectionLabel>
        <Button size="icon-xs" variant="ghost" title="Close" aria-label="Close" onClick={onClose}>
          <X />
        </Button>
      </div>
      {state.status === "loading" ? (
        <p className="px-2 py-1 text-xs text-muted-foreground">Reading git blame…</p>
      ) : state.status === "error" ? (
        <p className="mx-1 rounded-md bg-destructive/10 px-2 py-1.5 text-xs text-destructive">
          {state.message}
        </p>
      ) : state.result.commits.length === 0 ? (
        <p className="px-2 py-1 text-xs text-muted-foreground">
          {state.result.uncommitted
            ? "This line has changes that aren't committed yet, so no commit explains it."
            : "git blame named no commit for this line."}
        </p>
      ) : (
        <>
          {state.result.commits.map((c) => (
            <WhyCommitBlock key={c.sha} commit={c} />
          ))}
          {state.result.unlinked ? (
            <p className="px-2 py-1 text-xs text-muted-foreground">
              No session is linked to this commit yet. Run <code>cal commits</code> in the repo to
              link its history, then ask again.
            </p>
          ) : null}
        </>
      )}
    </section>
  );
}

export function SidebarApp({ init }: { init: InitPayload }) {
  const persisted = getState<Persisted>();
  const [query, setQuery] = useState(persisted?.query ?? init.query ?? "");
  const [scope, setScope] = useState<Scope>(persisted?.scope ?? "all");
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const [recent, setRecent] = useState<ThreadSummary[] | null>(null);
  const [stats, setStats] = useState<Stats | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [related, setRelated] = useState<ThreadSummary[]>([]);
  const [relatedLabel, setRelatedLabel] = useState("");
  const [why, setWhy] = useState<WhyState | null>(null);

  const project = init.projectPath ?? null;

  const runSearch = useCallback(
    async (q: string, sc: Scope) => {
      if (!q.trim()) {
        setHits(null);
        return;
      }
      setLoading(true);
      setError(null);
      try {
        const proj = sc === "project" ? (project ?? undefined) : undefined;
        setHits(await request("search", { query: q, project: proj }));
      } catch (e) {
        setError((e as Error).message);
      } finally {
        setLoading(false);
      }
    },
    [project],
  );

  const loadBase = useCallback(async () => {
    try {
      const [r, s] = await Promise.all([request("recent", {}), request("stats", {})]);
      setRecent(r);
      setStats(s);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  // Debounced live search.
  useEffect(() => {
    const t = setTimeout(() => runSearch(query, scope), 300);
    return () => clearTimeout(t);
  }, [query, scope, runSearch]);

  useEffect(() => {
    loadBase();
  }, [loadBase]);

  useEffect(() => {
    setState<Persisted>({ query, scope });
  }, [query, scope]);

  // Title-bar refresh button (host posts "refresh").
  useEffect(() => {
    onRefresh(() => {
      loadBase();
      runSearch(query, scope);
    });
  }, [loadBase, runSearch, query, scope]);

  // Ambient recall: the host pushes related threads for the current editor context.
  useEffect(() => {
    onRelated((label, results) => {
      setRelatedLabel(label);
      setRelated(results);
    });
  }, []);

  // "Why this line": the host pushes each state of the editor command.
  useEffect(() => {
    onWhy(setWhy);
  }, []);

  const showingResults = query.trim().length > 0;
  const summaryMeta = (t: ThreadSummary) =>
    [`${t.messageCount} msgs`, formatTime(t.updatedAt), shortPath(t.projectPath)]
      .filter(Boolean)
      .join(" · ");

  return (
    <div className="flex h-screen flex-col text-sm">
      <header className="flex-none space-y-2 px-2 pb-2 pt-2">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search your AI history…"
            className="bg-[var(--vscode-input-background)] pl-7"
          />
        </div>
        {project ? (
          <div className="flex gap-1">
            <Button
              size="xs"
              variant={scope === "all" ? "secondary" : "ghost"}
              onClick={() => setScope("all")}
            >
              All history
            </Button>
            <Button
              size="xs"
              variant={scope === "project" ? "secondary" : "ghost"}
              onClick={() => setScope("project")}
              title={project}
            >
              This project
            </Button>
          </div>
        ) : null}
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-1 pb-2">
        {error ? (
          <p className="mx-1 my-2 rounded-md bg-destructive/10 px-2 py-1.5 text-xs text-destructive">
            {error}
          </p>
        ) : null}

        {why ? (
          <WhySection
            state={why}
            onClose={() => {
              setWhy(null);
              closeWhy();
            }}
          />
        ) : null}

        {showingResults ? (
          <>
            <SectionLabel>{loading ? "Searching…" : `${hits?.length ?? 0} results`}</SectionLabel>
            {hits && hits.length > 0
              ? hits.map((h) => (
                  <Row
                    key={`${h.threadId}-${h.snippet.slice(0, 12)}`}
                    id={h.threadId}
                    source={h.source}
                    title={h.title}
                    meta={shortPath(h.projectPath)}
                    snippetHtml={renderSnippet(h.snippet)}
                  />
                ))
              : !loading && (
                  <p className="px-2 py-6 text-center text-xs text-muted-foreground">
                    No matching threads.
                  </p>
                )}
          </>
        ) : (
          <>
            {related.length > 0 ? (
              <>
                <SectionLabel>Related{relatedLabel ? ` · ${relatedLabel}` : ""}</SectionLabel>
                {related.map((t) => (
                  <Row
                    key={`related-${t.id}`}
                    id={t.id}
                    source={t.source}
                    title={t.title}
                    meta={summaryMeta(t)}
                  />
                ))}
              </>
            ) : null}

            <SectionLabel>Recent</SectionLabel>
            {recent && recent.length > 0 ? (
              recent.map((t) => (
                <Row key={t.id} id={t.id} source={t.source} title={t.title} meta={summaryMeta(t)} />
              ))
            ) : (
              <p className="px-2 py-6 text-center text-xs text-muted-foreground">
                No threads indexed yet. Open the Callimachus app once to build the index.
              </p>
            )}
          </>
        )}
      </div>

      {stats ? (
        <footer className="flex-none border-t border-border px-3 py-1.5 text-[11px] text-muted-foreground">
          {stats.threads.toLocaleString()} threads · {stats.messages.toLocaleString()} messages
        </footer>
      ) : null}
    </div>
  );
}
