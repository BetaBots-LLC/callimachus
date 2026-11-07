import { type ReactNode, useEffect, useId, useRef } from "react";
import { ChevronRight } from "lucide-react";
import { cn } from "@/lib/utils";
import { type ProjectSectionId, useProjectSections } from "./sections";

/**
 * One collapsible section of the Project page. The body mounts only while the section is
 * open, so each section's queries run only when someone looks at it. The header is a real
 * button inside the heading (Tab reaches every section; Enter or Space toggles it), and
 * `aside` sits on the header row for a count or one action that works while collapsed.
 */
export function ProjectSection({
  id,
  title,
  hint,
  aside,
  children,
}: {
  id: ProjectSectionId;
  title: string;
  /** One line on what the section holds, so a collapsed section still says what it is. */
  hint: string;
  aside?: ReactNode;
  children: ReactNode;
}) {
  const open = useProjectSections((s) => s.open.has(id));
  const toggle = useProjectSections((s) => s.toggle);
  const focus = useProjectSections((s) => s.focus);
  const focusSeq = useProjectSections((s) => s.focusSeq);
  const clearFocus = useProjectSections((s) => s.clearFocus);
  const ref = useRef<HTMLElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const stopFollow = useRef<() => void>(() => {});
  const bodyId = useId();

  // A link asked for this section: bring it into view and put focus on its header, so the
  // keyboard continues from where the eye lands instead of the top of the page. The follow
  // outlives this effect (clearFocus re-runs it at once), so it stops on its own or on unmount.
  // biome-ignore lint/correctness/useExhaustiveDependencies: focusSeq re-runs it for a repeat request
  useEffect(() => {
    if (focus !== id) return;
    clearFocus();
    const el = ref.current;
    if (!el) return;
    stopFollow.current();
    stopFollow.current = followIntoView(el);
    buttonRef.current?.focus({ preventScroll: true });
  }, [focus, focusSeq, id, clearFocus]);
  useEffect(() => () => stopFollow.current(), []);

  return (
    // The scroll margin is the page's sticky header (measured by ProjectPage), so a revealed
    // section's header row, which holds the focus, is not hidden under it.
    <section
      ref={ref}
      aria-labelledby={`${bodyId}-title`}
      className="scroll-mt-[calc(var(--page-header-h,4.5rem)+0.25rem)]"
    >
      <div className="flex min-h-9 items-center gap-2">
        <h2 id={`${bodyId}-title`} className="min-w-0 flex-1">
          <button
            ref={buttonRef}
            type="button"
            aria-expanded={open}
            // Only while the body is mounted: a collapsed section has nothing to point at.
            aria-controls={open ? bodyId : undefined}
            // The hint is a description, so the button's name stays the section title.
            aria-describedby={`${bodyId}-hint`}
            onClick={() => toggle(id)}
            className="flex w-full min-w-0 cursor-pointer items-baseline gap-2 rounded-md py-1.5 pr-2 pl-1 text-left outline-none transition-colors hover:bg-muted/50 focus-visible:ring-[3px] focus-visible:ring-ring/50"
          >
            <ChevronRight
              aria-hidden="true"
              className={cn(
                "size-4 shrink-0 translate-y-[3px] text-muted-foreground transition-transform duration-150",
                open && "rotate-90",
              )}
            />
            <span className="shrink-0 text-sm font-semibold text-foreground">{title}</span>
            <span
              id={`${bodyId}-hint`}
              aria-hidden="true"
              className="min-w-0 truncate text-[13px] text-muted-foreground"
            >
              {hint}
            </span>
          </button>
        </h2>
        {aside && <div className="flex shrink-0 items-center gap-1.5">{aside}</div>}
      </div>
      {open && (
        <div id={bodyId} className="pt-1 pb-2 pl-1">
          {children}
        </div>
      )}
    </section>
  );
}

/** How long a revealed section keeps re-aligning while the sections above it load. Their
 *  skeletons turn into lists well within a second on a large index; two covers a slow
 *  aggregate without fighting someone who has started reading. */
const FOLLOW_MS = 2000;

/** Scroll `el` to the top of its scroller, then keep it there while the sections above it
 *  settle: a single scroll on mount lands on whatever grows above it (Today's snapshot link
 *  used to land on Memory). Ends after FOLLOW_MS, or as soon as the reader scrolls, clicks or
 *  types. Returns the stop function. */
function followIntoView(el: HTMLElement): () => void {
  el.scrollIntoView({ block: "start", behavior: "smooth" });
  const list = el.parentElement;
  if (!list || typeof ResizeObserver === "undefined") return () => {};
  let height = list.getBoundingClientRect().height;
  const ro = new ResizeObserver(() => {
    const now = list.getBoundingClientRect().height;
    if (now === height) return; // the first callback, on observe
    height = now;
    el.scrollIntoView({ block: "start", behavior: "smooth" });
  });
  ro.observe(list);
  const events = ["wheel", "touchstart", "pointerdown", "keydown"] as const;
  const stop = () => {
    ro.disconnect();
    clearTimeout(timer);
    for (const e of events) window.removeEventListener(e, stop, true);
  };
  const timer = setTimeout(stop, FOLLOW_MS);
  for (const e of events) window.addEventListener(e, stop, { capture: true, passive: true });
  return stop;
}

/** A section body's quiet one-paragraph state (empty, loading failed). */
export function SectionNote({
  children,
  tone = "muted",
}: {
  children: ReactNode;
  tone?: "muted" | "error";
}) {
  return (
    <p
      role={tone === "error" ? "alert" : undefined}
      className={cn(
        "px-1 py-1 text-sm",
        tone === "error" ? "text-destructive" : "text-muted-foreground",
      )}
    >
      {children}
    </p>
  );
}

/** Placeholder rows while a section loads: the list's shape, not a spinner in the middle. */
export function SectionSkeleton({ rows = 3 }: { rows?: number }) {
  return (
    <div
      role="status"
      aria-busy="true"
      aria-label="Loading"
      className="divide-y divide-border overflow-hidden rounded-xl border bg-card"
    >
      {Array.from({ length: rows }, (_, i) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: static placeholders, never reordered
        <div key={i} className="space-y-2 px-3 py-3">
          <div className="h-3.5 w-2/3 animate-pulse rounded bg-muted" />
          <div className="h-3 w-1/3 animate-pulse rounded bg-muted/70" />
        </div>
      ))}
    </div>
  );
}

/** The bordered list every section uses, so rows read as one family across the page. */
export function RowList({ children, label }: { children: ReactNode; label?: string }) {
  return (
    <ul
      aria-label={label}
      className="divide-y divide-border overflow-hidden rounded-xl border bg-card"
    >
      {children}
    </ul>
  );
}
