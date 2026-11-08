import { useQuery } from "@tanstack/react-query";
import { GitCommitHorizontal, Search } from "lucide-react";
import { api, type IssueFix } from "../../lib/api";
import { AGGREGATE_STALE_MS } from "../../lib/queryClient";
import { errorText } from "../../lib/toast";
import { useUi } from "../../store/ui";
import { RowList, SectionNote, SectionSkeleton } from "./ProjectSection";
import { fullDate, relativeAge, useOpenThread } from "./shared";

/** The errors this repo's sessions keep hitting (last 180 days, grouped and counted by the
 *  backend), or with no project the errors that recur across every repo (what the Coach tab
 *  listed, now under All projects). Each row searches for the error, so the sessions behind
 *  the count are one click away; one a linked commit fixed also links the session that
 *  fixed it. */
export function ErrorsSection({ project = null }: { project?: string | null }) {
  const openSearch = useUi((s) => s.openSearch);
  const issues = useQuery({
    queryKey: ["recurring_issues", project],
    queryFn: () => api.recurringIssues(project ?? undefined),
    staleTime: AGGREGATE_STALE_MS, // refreshed by index events (main.tsx)
  });

  if (issues.isPending) return <SectionSkeleton rows={2} />;
  if (issues.isError) {
    return (
      <SectionNote tone="error">
        Couldn't load the recurring errors: {errorText(issues.error)}
      </SectionNote>
    );
  }
  if (issues.data.length === 0) {
    return (
      <SectionNote>
        {project
          ? "No error has come up in more than one session of this repo in the last 180 days."
          : "No error has come up in more than one session in the last 180 days."}
      </SectionNote>
    );
  }
  return (
    <RowList label="Recurring errors">
      {issues.data.slice(0, 10).map((i) => (
        <li key={i.example}>
          <button
            type="button"
            // Scoped to this repo (or every repo) and all time, so the list shows the sessions
            // the count is made of rather than whatever scope and date preset Search had.
            onClick={() => openSearch({ query: i.example, project, after: null, before: null })}
            title={project ? "Search this repo for this error" : "Search every repo for this error"}
            className={`flex w-full cursor-pointer items-center gap-3 px-3 text-left outline-none transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50 ${i.fix ? "pt-2.5 pb-1.5" : "py-2.5"}`}
          >
            <span className="min-w-0 flex-1">
              <code className="block truncate font-mono text-xs text-foreground">{i.example}</code>
              <span className="mt-1 block text-[13px] text-muted-foreground">
                <span className="font-medium tabular-nums text-foreground">{i.count}</span> times in{" "}
                {i.threads} {i.threads === 1 ? "session" : "sessions"} · last seen{" "}
                <span title={fullDate(i.lastSeen)}>{relativeAge(i.lastSeen)}</span>
              </span>
            </span>
            <Search aria-hidden="true" className="size-3.5 shrink-0 text-muted-foreground" />
          </button>
          {i.fix && <FixLink fix={i.fix} playbook={i.playbook ?? null} />}
        </li>
      ))}
    </RowList>
  );
}

/** "Last fixed in <sha> (<age>)": opens the session that hit the error and committed the
 *  fix. The playbook line recorded for it is the tooltip. */
function FixLink({ fix, playbook }: { fix: IssueFix; playbook: string | null }) {
  const openThread = useOpenThread();
  return (
    <button
      type="button"
      onClick={() => openThread(fix.threadId)}
      title={playbook ?? "Open the session that fixed it"}
      className="flex w-full cursor-pointer items-center gap-1.5 px-3 pb-2.5 text-left text-[13px] text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:text-foreground focus-visible:ring-[3px] focus-visible:ring-inset focus-visible:ring-ring/50"
    >
      <GitCommitHorizontal aria-hidden="true" className="size-3.5 shrink-0" />
      <span className="min-w-0 truncate">
        Last fixed in <code className="font-mono text-xs text-foreground">{fix.shortSha}</code> (
        <span title={fullDate(fix.committedAt)}>{relativeAge(fix.committedAt)}</span>)
        {fix.subject ? `: ${fix.subject}` : ""}
      </span>
    </button>
  );
}
