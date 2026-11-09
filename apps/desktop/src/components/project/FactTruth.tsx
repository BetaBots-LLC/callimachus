import type { ReactNode } from "react";
import {
  SOURCE_LABELS,
  type FactFlag,
  type LinkKind,
  type LinkOutcome,
  type MemoryFact,
} from "../../lib/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

/**
 * Where a fact stands and where it came from, as the Memory section shows it: the session
 * date and agent (or who recorded it), and badges for what it replaces, who disputes it and
 * how many sessions confirmed it. The injected memory prints the same facts as
 * `- text (2026-10-03, codex, thread 1234; replaces 2026-10-01)`; this is that line for people.
 */

/** A fact's date as the page prints it: the day only, in the viewer's locale but on the UTC
 *  calendar, the day the agents' memory lines (`fact_date`) print, so the two never differ
 *  near midnight. */
export function factDate(epochSeconds: number): string {
  if (!epochSeconds) return "";
  return new Date(epochSeconds * 1000).toLocaleDateString([], {
    year: "numeric",
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  });
}

/** Who a fact is from: the session's agent for a distilled fact, else who recorded it. Null
 *  for facts the row already labels with a badge (repo imports, Claude Code memory). */
export function factOrigin(f: MemoryFact): string | null {
  switch (f.extractor) {
    case "repo":
    case "claude-memory":
      return null;
    case "user":
      return "Recorded by you";
    case "agent":
      return "Recorded by an agent";
    default:
      // A recorded fact's thread is the project's notes thread (`in_app`), not a chat.
      return f.source === "in_app" ? "Recorded" : (SOURCE_LABELS[f.source] ?? f.source);
  }
}

/** The sentence for a flag: "Flagged stale by an agent on Oct 3, 2026". */
export function flagLine(flag: FactFlag): string {
  const who =
    flag.source === "agent" ? "an agent" : flag.source === "distill" ? "a memory pass" : "you";
  return `Flagged ${flag.kind} by ${who} on ${factDate(flag.createdAt)}`;
}

/** What to tell the user when a link they asked for was refused. */
export function linkRefusal(o: LinkOutcome): string | null {
  if (o.outcome !== "rejected") return null;
  switch (o.reason) {
    case "notFound":
      return "One of the two facts is gone or hidden.";
    case "notLinkable":
    case "kindMismatch":
      return "Only two decisions, or two gotchas, can replace each other.";
    case "crossProject":
      return "The two facts belong to different projects.";
    case "selfLink":
      return "A fact can't replace itself.";
    case "cycle":
      return "The other one already replaces this one. Undo that first.";
    case "alreadySettled":
      return "One of them already replaces the other.";
    default:
      return "Couldn't link them.";
  }
}

/** The muted line under a fact: date, origin, then the badges for where it stands. The
 *  Disputed badge toggles the dispute panel (`onToggleDispute`). */
export function FactMeta({
  fact,
  lead,
  disputeOpen,
  onToggleDispute,
}: {
  fact: MemoryFact;
  /** Shown first: the row's own badges (From the repo, Shared) and memory file name. */
  lead?: ReactNode;
  disputeOpen: boolean;
  onToggleDispute: () => void;
}) {
  const origin = factOrigin(fact);
  const disputed = fact.disputedBy ?? [];
  const date = factDate(fact.decidedAt);
  return (
    <p className="mt-0.5 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted-foreground">
      {lead}
      {(date || origin) && (
        <span className="shrink-0 tabular-nums">{[date, origin].filter(Boolean).join(" · ")}</span>
      )}
      {fact.title && fact.source !== "in_app" && (
        <span className="line-clamp-1 min-w-0">
          {(date || origin) && <span aria-hidden>· </span>}
          {fact.title}
        </span>
      )}
      {fact.replaces != null && (
        <Badge
          variant="outline"
          className="shrink-0 text-[0.65rem]"
          title="This replaced an earlier decision, which memory now leaves out"
        >
          {factDate(fact.replaces) ? `Replaces ${factDate(fact.replaces)}` : "Replaces another"}
        </Badge>
      )}
      {disputed.length > 0 && (
        <Badge
          variant="outline"
          className="shrink-0 cursor-pointer border-amber-500/50 text-[0.65rem] text-amber-700 dark:text-amber-400"
          title={disputed.map((d) => `${factDate(d.decidedAt)}: ${d.text}`).join("\n")}
          render={
            <button
              type="button"
              aria-expanded={disputeOpen}
              onClick={(e) => {
                e.stopPropagation();
                onToggleDispute();
              }}
              onKeyDown={(e) => e.stopPropagation()}
            />
          }
        >
          Disputed
        </Badge>
      )}
      {fact.status === "flagged" && (
        <Badge
          variant="outline"
          className="shrink-0 border-amber-500/50 text-[0.65rem] text-amber-700 dark:text-amber-400"
          title={
            fact.flags?.[0]
              ? `${flagLine(fact.flags[0])}${fact.flags[0].reason ? `: ${fact.flags[0].reason}` : ""}. Pinned, so it stays in memory until you keep or hide it.`
              : undefined
          }
        >
          Flagged
        </Badge>
      )}
      {fact.support > 1 && (
        <Badge
          variant="outline"
          className="shrink-0 text-[0.65rem] tabular-nums"
          title={
            fact.lastConfirmed ? `Last confirmed on ${factDate(fact.lastConfirmed)}` : undefined
          }
        >
          Confirmed in {fact.support} sessions
        </Badge>
      )}
    </p>
  );
}

/** The other side of each dispute, with the three ways to settle it. Clicks stay here: the
 *  row around it opens the fact's thread. */
export function DisputePanel({
  fact,
  onLink,
  onUnlink,
}: {
  fact: MemoryFact;
  onLink: (fromId: number, toId: number, kind: LinkKind) => void;
  onUnlink: (fromId: number, toId: number, kind: LinkKind) => void;
}) {
  const others = fact.disputedBy ?? [];
  if (!others.length) return null;
  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: only stops clicks reaching the row
    <div
      className="mt-2 space-y-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-2 text-xs"
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      {others.map((o) => (
        <div key={o.id} className="space-y-1.5">
          <p className="text-muted-foreground">
            Disputed by a fact from {factDate(o.decidedAt)}
            {o.threadId != null && <> (thread {o.threadId})</>}:
          </p>
          <p className="text-foreground">{o.text}</p>
          <div className="flex flex-wrap gap-1">
            <Button size="xs" variant="outline" onClick={() => onLink(fact.id, o.id, "supersedes")}>
              Keep this one
            </Button>
            <Button size="xs" variant="outline" onClick={() => onLink(o.id, fact.id, "supersedes")}>
              Keep the other
            </Button>
            <Button
              size="xs"
              variant="ghost"
              title="They don't conflict: drop the dispute and keep both"
              onClick={() => onUnlink(fact.id, o.id, "contradicts")}
            >
              Both stand
            </Button>
          </div>
        </div>
      ))}
    </div>
  );
}
