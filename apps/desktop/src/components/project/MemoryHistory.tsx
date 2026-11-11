import { useState } from "react";
import { ChevronRight } from "lucide-react";
import type { MemoryFact } from "../../lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { dateTitle, factDate, flaggedStanding, flagLine } from "./FactTruth";

/**
 * The Memory section's history: facts that are out of injected memory but kept, so the user
 * can see why and change their mind. "Replaced" holds facts a newer one replaced (Undo
 * restores the fact: every replacement of it is undone at once and stays undone, however
 * many facts replaced it); "Flagged" holds facts an agent, a memory pass or the user marked
 * stale or wrong (Keep returns one to memory, Hide drops it).
 */

function GroupHeading({ title, count }: { title: string; count: number }) {
  return (
    <>
      {title}
      <span className="font-mono text-[0.65rem] tabular-nums text-muted-foreground/60">
        {count}
      </span>
    </>
  );
}

const headingClass =
  "flex items-baseline gap-2 px-1 text-[0.7rem] font-medium uppercase tracking-wider text-muted-foreground";

/** Facts a newer one replaced, collapsed by default: history, not memory. */
export function ReplacedGroup({
  facts,
  onUndo,
}: {
  facts: MemoryFact[];
  /** Restore the replaced fact: undo every replacement of it. */
  onUndo: (id: number) => void;
}) {
  const [open, setOpen] = useState(false);
  if (!facts.length) return null;
  return (
    <section>
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className={cn(
          headingClass,
          "mb-2 cursor-pointer rounded-sm outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50",
        )}
      >
        <ChevronRight
          aria-hidden
          className={cn("size-3 self-center transition-transform", open && "rotate-90")}
        />
        <GroupHeading title="Replaced" count={facts.length} />
      </button>
      {open && (
        <div className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
          {facts.map((f) => (
            <div key={f.id} className="flex items-start gap-2 px-3 py-2.5">
              <div className="min-w-0 flex-1">
                <p className="text-sm leading-relaxed text-muted-foreground">{f.text}</p>
                <p className="mt-0.5 text-[13px] text-muted-foreground">
                  <span className="tabular-nums" title={dateTitle(f.decidedAt)}>
                    {factDate(f.decidedAt)}
                  </span>
                  {f.supersededBy && (
                    <>
                      {" · "}replaced by{" "}
                      <span className="text-foreground">{f.supersededBy.text}</span> on{" "}
                      <span className="tabular-nums" title={dateTitle(f.supersededBy.decidedAt)}>
                        {factDate(f.supersededBy.decidedAt)}
                      </span>
                    </>
                  )}
                </p>
              </div>
              {f.supersededBy && (
                <Button
                  size="xs"
                  variant="ghost"
                  className="shrink-0"
                  title="Undo this replacement: the fact returns to memory, whatever else replaced it."
                  onClick={() => onUndo(f.id)}
                >
                  Undo
                </Button>
              )}
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

/** Facts with an unresolved stale / wrong flag. Shown only when there are any. */
export function FlaggedGroup({
  facts,
  onKeep,
  onHide,
}: {
  facts: MemoryFact[];
  onKeep: (id: number) => void;
  onHide: (id: number) => void;
}) {
  if (!facts.length) return null;
  return (
    <section>
      <h3 className={cn(headingClass, "mb-2")}>
        <GroupHeading title="Flagged" count={facts.length} />
      </h3>
      <div className="divide-y divide-border overflow-hidden rounded-xl border border-amber-500/40 bg-card">
        {facts.map((f) => {
          const flag = f.flags?.[0];
          return (
            <div key={f.id} className="flex items-start gap-2 px-3 py-2.5">
              <div className="min-w-0 flex-1">
                <p className="text-sm leading-relaxed text-foreground">{f.text}</p>
                {flag && (
                  <p className="mt-0.5 text-[13px] text-muted-foreground">
                    {flagLine(flag)}
                    {flag.reason && (
                      <>
                        : <span className="text-foreground">{flag.reason}</span>
                      </>
                    )}
                  </p>
                )}
                <p className="mt-0.5 text-[13px] text-muted-foreground">
                  {flaggedStanding(f)} until you choose.
                </p>
              </div>
              <div className="flex shrink-0 gap-1">
                <Button
                  size="xs"
                  variant="outline"
                  title="It still holds: clear the flag"
                  onClick={() => onKeep(f.id)}
                >
                  Keep
                </Button>
                <Button
                  size="xs"
                  variant="ghost"
                  title="Hide from memory"
                  onClick={() => onHide(f.id)}
                >
                  Hide
                </Button>
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}
