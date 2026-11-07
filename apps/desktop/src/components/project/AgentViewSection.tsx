import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { CircleAlert, VolumeX } from "lucide-react";
import { api } from "../../lib/api";
import { baseName } from "../../lib/format";
import { errorText } from "../../lib/toast";
import { useUi } from "../../store/ui";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { SectionNote, SectionSkeleton } from "./ProjectSection";
import { CopyButton } from "./shared";

/** Exactly what an agent starting in this repo receives: the SessionStart hook's output
 *  for a new session (the same render `cal hook` prints, so the preview cannot drift) and
 *  the managed block `cal agents` keeps in AGENTS.md. Read-only. */
export function AgentViewSection({ project }: { project: string }) {
  const openSettings = useUi((s) => s.openSettings);
  const view = useQuery({
    queryKey: ["agent_view", project],
    queryFn: () => api.agentView(project),
  });

  if (view.isPending) return <SectionSkeleton rows={2} />;
  if (view.isError) {
    return (
      <SectionNote tone="error">Couldn't build the preview: {errorText(view.error)}</SectionNote>
    );
  }
  // The install and file-state lines test for an exact value (`=== false`, `=== "stale"`):
  // an older backend leaves both fields out, and then the page says nothing rather than guess.
  const v = view.data;
  const silent = v.hookSilentReason ? silentReason(v.hookSilentReason) : null;
  return (
    <div className="space-y-4">
      {/* When the hook is silent because of the mute, its own sentence says so. */}
      {v.muted && silent?.code !== "muted" && (
        <Notice icon={VolumeX}>
          The recent-sessions digest is muted for this repo, so a new session gets its memory only,
          never the digest. Turn it back on with <Cmd>cal hook --unmute</Cmd> in the repo.
        </Notice>
      )}
      <CodePane
        title="SessionStart hook"
        detail="What a new session in this repo gets before its first prompt."
        text={v.hook}
        chars={v.hookChars}
        // The backend's cap (Claude Code's 10,000-char limit on a hook's injected context).
        cap={v.hookCap}
        status={
          v.hookInstalled === false && (
            <StatusLine
              action={
                <Button size="xs" variant="outline" onClick={() => openSettings("agents")}>
                  Open Agents settings
                </Button>
              }
            >
              Claude Code's hook is not installed, so no session gets this.
            </StatusLine>
          )
        }
        empty={
          silent ? (
            <>
              {silent.text}
              {silent.code === "digest-off" && (
                <Button
                  size="xs"
                  variant="link"
                  className="ml-1 h-auto p-0 align-baseline"
                  onClick={() => openSettings("agents")}
                >
                  Open Agents settings
                </Button>
              )}
            </>
          ) : (
            "Nothing yet: this repo has no memory or recent sessions to inject."
          )
        }
      />
      <CodePane
        title="AGENTS.md block"
        detail="The managed block Update AGENTS.md (Memory, More) writes into the repo."
        text={v.agentsFile}
        chars={v.agentsChars}
        status={
          v.agentsFileState === "stale" && (
            <StatusLine>
              AGENTS.md holds an older block. Update AGENTS.md (Memory, More) writes this one.
            </StatusLine>
          )
        }
        empty="Nothing to write yet: the block holds decisions, gotchas and TODOs, and this repo has none."
      />
    </div>
  );
}

type SilentCode = "managed" | "muted" | "digest-off" | "no-threads" | "other";

/** The hook's reason for silence, as app copy. The backend gives the `cal hook --explain`
 *  wording ("muted: no memory yet and the digest is muted for /path (`cal hook --unmute` to
 *  undo)"), which reads as CLI output pasted into a sentence, so each reason it can give for
 *  a new session maps to a sentence of its own. An unknown one is shown as given. */
function silentReason(reason: string): { code: SilentCode; text: ReactNode } {
  const managed = /^managed block found in (.+?) \(/.exec(reason);
  if (managed) {
    return {
      code: "managed",
      text: `Silent for a new session here: ${baseName(managed[1])} already carries the memory block, and Claude Code loads that file itself.`,
    };
  }
  if (reason.startsWith("muted:")) {
    return {
      code: "muted",
      text: (
        <>
          Silent for a new session here: this repo has no memory yet, and its recent-sessions digest
          is muted. Run <Cmd>cal hook --unmute</Cmd> in the repo to turn the digest back on.
        </>
      ),
    };
  }
  if (reason.startsWith("digest off:")) {
    return {
      code: "digest-off",
      text: "Silent for a new session here: this repo has no memory yet, and the recent-sessions digest is off.",
    };
  }
  if (reason.startsWith("no threads:")) {
    return {
      code: "no-threads",
      text: "Silent for a new session here: no session from this repo is indexed yet.",
    };
  }
  return { code: "other", text: `Silent for a new session here: ${reason.replace(/\.$/, "")}.` };
}

function Cmd({ children }: { children: ReactNode }) {
  return <code className="font-mono text-xs whitespace-nowrap text-foreground">{children}</code>;
}

function Notice({ icon: Icon, children }: { icon: typeof VolumeX; children: ReactNode }) {
  return (
    <div className="flex items-start gap-2 rounded-lg border bg-muted/40 px-3 py-2 text-sm">
      <Icon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
      <p className="text-muted-foreground">{children}</p>
    </div>
  );
}

/** One line under a pane's caption on whether agents actually get it. */
function StatusLine({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <p className="mb-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 px-1 text-[13px] text-amber-700 dark:text-amber-400">
      <CircleAlert aria-hidden="true" className="size-3.5 shrink-0" />
      <span>{children}</span>
      {action}
    </p>
  );
}

function CodePane({
  title,
  detail,
  text,
  chars,
  cap,
  status,
  empty,
}: {
  title: string;
  detail: string;
  text: string;
  chars: number;
  cap?: number;
  /** Whether agents really get this pane's text (hook not installed, an older block). */
  status?: ReactNode;
  empty: ReactNode;
}) {
  const over = cap !== undefined && chars > cap;
  return (
    <figure className="min-w-0">
      <figcaption className="mb-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 px-1">
        <span className="text-sm font-medium">{title}</span>
        <span
          className={cn(
            "text-xs tabular-nums",
            over ? "font-medium text-destructive" : "text-muted-foreground",
          )}
        >
          {chars.toLocaleString()}
          {cap !== undefined ? ` of ${cap.toLocaleString()} chars` : " chars"}
          {over ? ", over the cap: the agent sees it cut off" : ""}
        </span>
        <span className="ml-auto flex items-center gap-2">
          <CopyButton text={text} disabled={!text} />
        </span>
        <span className="basis-full text-[13px] text-muted-foreground">{detail}</span>
      </figcaption>
      {status}
      {text ? (
        <pre
          // Focusable so the keyboard can scroll a long preview.
          // biome-ignore lint/a11y/noNoninteractiveTabindex: a scroll region needs focus to scroll by keyboard
          tabIndex={0}
          className="max-h-80 overflow-auto rounded-xl border bg-muted/30 px-3 py-2.5 font-mono text-xs leading-relaxed whitespace-pre-wrap wrap-break-word text-foreground outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50"
        >
          {text}
        </pre>
      ) : (
        <p className="rounded-xl border border-dashed px-3 py-3 text-sm text-muted-foreground">
          {empty}
        </p>
      )}
    </figure>
  );
}
