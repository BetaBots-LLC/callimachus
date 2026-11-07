import { useMutation, useQuery } from "@tanstack/react-query";
import { ArrowUpRight, GitBranch, Terminal } from "lucide-react";
import { api, SOURCE_LABELS, type ThreadSummary } from "../../lib/api";
import { errorText } from "../../lib/toast";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { RowList, SectionNote, SectionSkeleton } from "./ProjectSection";
import {
  AgentMenu,
  fullDate,
  recentSessionsQuery,
  relativeAge,
  useCanLaunchCli,
  useOpenThread,
} from "./shared";

/** The project's last sessions, newest first, each one click from its transcript, a resumed
 *  agent session, or a fresh agent seeded with its context. */
export function RecentSessions({ project }: { project: string }) {
  const recent = useQuery(recentSessionsQuery(project));
  const canLaunch = useCanLaunchCli();

  if (recent.isPending) return <SectionSkeleton rows={4} />;
  if (recent.isError) {
    return (
      <SectionNote tone="error">
        Couldn't load the recent sessions: {errorText(recent.error)}
      </SectionNote>
    );
  }
  if (recent.data.length === 0) {
    return (
      <SectionNote>
        No sessions in this repo yet. Sessions show up here once an agent works in it and the index
        catches up.
      </SectionNote>
    );
  }
  return (
    <RowList label="Recent sessions">
      {recent.data.map((t) => (
        <SessionRow key={t.id} thread={t} canLaunch={canLaunch} />
      ))}
    </RowList>
  );
}

function SessionRow({ thread, canLaunch }: { thread: ThreadSummary; canLaunch: boolean }) {
  const openThread = useOpenThread();
  // Native resume exists only for the agents whose CLI takes a session id.
  const canResume = canLaunch && (thread.source === "claude_code" || thread.source === "codex");
  const resume = useMutation({ mutationFn: () => api.resumeThread(thread.id) });
  const openIn = useMutation({
    mutationFn: (program: string) => api.openThreadInCli(thread.id, program),
  });
  const failure = resume.isError
    ? `Couldn't resume the session: ${errorText(resume.error)}`
    : openIn.isError
      ? `Couldn't open a terminal: ${errorText(openIn.error)}`
      : null;

  return (
    <li className="group/row flex items-center gap-2 px-1.5 py-1 transition-colors hover:bg-muted/40 has-[>button:focus-visible]:bg-muted/40">
      <button
        type="button"
        onClick={() => openThread(thread.id)}
        title="Open the transcript"
        className="min-w-0 flex-1 cursor-pointer rounded-md px-1.5 py-1.5 text-left outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50"
      >
        <span className="block truncate text-sm text-foreground">
          {thread.title || "Untitled session"}
        </span>
        <span className="mt-0.5 flex min-w-0 items-center gap-1.5 text-[13px] text-muted-foreground">
          <span className="shrink-0">{SOURCE_LABELS[thread.source]}</span>
          {thread.gitBranch && (
            <span className="flex min-w-0 items-center gap-1" title={thread.gitBranch}>
              <span aria-hidden="true">·</span>
              <GitBranch aria-hidden="true" className="size-3 shrink-0" />
              <span className="truncate font-mono text-xs">{thread.gitBranch}</span>
            </span>
          )}
          <span className="shrink-0 tabular-nums" title={fullDate(thread.updatedAt)}>
            · {relativeAge(thread.updatedAt)}
          </span>
          <span className="shrink-0 tabular-nums">
            · {thread.messageCount} {thread.messageCount === 1 ? "msg" : "msgs"}
          </span>
        </span>
        {failure && (
          <span role="alert" className="mt-1 block text-xs text-destructive">
            {failure}
          </span>
        )}
      </button>
      {canLaunch && (
        <div className="flex shrink-0 items-center gap-1 pr-1.5">
          {canResume && (
            <Button
              size="xs"
              variant="outline"
              onClick={() => resume.mutate()}
              disabled={resume.isPending}
              title={`Resume this session in ${SOURCE_LABELS[thread.source]}`}
            >
              {resume.isPending ? (
                <>
                  <Spinner data-icon="inline-start" className="size-3" />
                  Resuming…
                </>
              ) : (
                <>
                  Resume
                  <ArrowUpRight data-icon="inline-end" />
                </>
              )}
            </Button>
          )}
          <AgentMenu
            label="Open with this context in…"
            onPick={(program) => openIn.mutate(program)}
            trigger={
              <Button
                size="icon-xs"
                variant="ghost"
                title="Open in CLI"
                aria-label={`Open ${thread.title || "this session"} in a CLI agent`}
                disabled={openIn.isPending}
              >
                {openIn.isPending ? <Spinner className="size-3" /> : <Terminal />}
              </Button>
            }
          />
        </div>
      )}
    </li>
  );
}
