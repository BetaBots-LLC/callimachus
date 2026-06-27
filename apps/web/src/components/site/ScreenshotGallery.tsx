import { useCallback, useEffect, useState } from "react";
import { ChevronLeft, ChevronRight, Expand, X } from "lucide-react";
import { SCREENSHOTS, type Shot } from "@/content/screenshots";
import { cn } from "@/lib/utils";

const PLATE_SHADOW = "shadow-[0_24px_60px_-20px_oklch(0.1_0.02_50/0.7)]";

/**
 * The "plates" — product figures as a featured hero plate plus a two-up grid, each click-to-enlarge
 * into a lightbox for exact detail. The lightbox carries legibility, so the grid can stay compact
 * without the screens becoming unreadable. Styled as catalogue figures (mono figure number, serif
 * title, one-line description below the plate) to match the reading-room aesthetic.
 */
export function ScreenshotGallery({ className, label }: { className?: string; label?: string }) {
  const shots = SCREENSHOTS;
  const [open, setOpen] = useState<number | null>(null);
  const [shown, setShown] = useState(false);

  const move = useCallback(
    (delta: number) => setOpen((i) => (i === null ? i : (i + delta + shots.length) % shots.length)),
    [shots.length],
  );

  useEffect(() => {
    if (open === null) {
      setShown(false);
      return;
    }
    const raf = requestAnimationFrame(() => setShown(true)); // mount-then-transition for a clean fade/scale
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(null);
      else if (e.key === "ArrowRight") move(1);
      else if (e.key === "ArrowLeft") move(-1);
    };
    document.addEventListener("keydown", onKey);
    document.body.style.overflow = "hidden";
    return () => {
      cancelAnimationFrame(raf);
      document.removeEventListener("keydown", onKey);
      document.body.style.overflow = "";
    };
  }, [open, move]);

  if (shots.length === 0) return null;

  const [featured, ...rest] = shots;
  // Keep rows balanced: an odd trailing plate spans the full width instead of leaving a gap.
  const fillsRow = rest.length % 2 === 1;

  return (
    <div className={cn("flex flex-col gap-12 sm:gap-14", className)}>
      {label && (
        <div className="flex items-center gap-4">
          <span className="cat-label text-primary">{label}</span>
          <span className="h-px flex-1 bg-border" />
        </div>
      )}

      <Plate shot={featured} onOpen={() => setOpen(0)} />

      {rest.length > 0 && (
        <div className="grid grid-cols-1 gap-x-6 gap-y-12 sm:grid-cols-2 sm:gap-y-14">
          {rest.map((s, i) => (
            <Plate
              key={s.file}
              shot={s}
              onOpen={() => setOpen(i + 1)}
              className={fillsRow && i === rest.length - 1 ? "sm:col-span-2" : undefined}
            />
          ))}
        </div>
      )}

      {open !== null && (
        <div
          role="dialog"
          aria-modal="true"
          aria-label={shots[open].alt}
          className={cn(
            "fixed inset-0 z-50 flex flex-col items-center justify-center gap-5 bg-background/92 p-4 backdrop-blur-sm transition-opacity duration-200 sm:p-10",
            shown ? "opacity-100" : "opacity-0",
          )}
        >
          {/* Backdrop: a real button so empty-space clicks close it without onClick-on-div a11y issues. */}
          <button
            type="button"
            aria-label="Close enlarged figure"
            onClick={() => setOpen(null)}
            className="absolute inset-0 cursor-default"
          />

          <button
            type="button"
            onClick={() => setOpen(null)}
            aria-label="Close"
            className="absolute right-5 top-5 z-10 flex size-9 items-center justify-center rounded-full border border-border bg-card text-muted-foreground transition-colors hover:text-foreground"
          >
            <X className="size-4" />
          </button>

          <img
            src={shots[open].file}
            alt={shots[open].alt}
            className={cn(
              "relative z-10 max-h-[82vh] w-auto max-w-[94vw] rounded-lg border border-border shadow-2xl transition-[transform,opacity] duration-300 ease-[var(--ease-out-quint)]",
              shown ? "scale-100 opacity-100" : "scale-[0.97] opacity-0",
            )}
          />

          <div className="relative z-10 flex items-center gap-4">
            {shots.length > 1 && (
              <button
                type="button"
                onClick={() => move(-1)}
                aria-label="Previous figure"
                className="flex size-8 items-center justify-center rounded-full border border-border bg-card text-muted-foreground transition-colors hover:text-foreground"
              >
                <ChevronLeft className="size-4" />
              </button>
            )}
            <p className="flex items-baseline gap-2 font-mono text-xs text-muted-foreground">
              <span className="text-primary">{shots[open].fig}</span>
              <span>{shots[open].title}</span>
            </p>
            {shots.length > 1 && (
              <button
                type="button"
                onClick={() => move(1)}
                aria-label="Next figure"
                className="flex size-8 items-center justify-center rounded-full border border-border bg-card text-muted-foreground transition-colors hover:text-foreground"
              >
                <ChevronRight className="size-4" />
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/** One catalogue plate: the framed, click-to-enlarge screenshot with its figure label below. */
function Plate({
  shot,
  onOpen,
  className,
}: {
  shot: Shot;
  onOpen: () => void;
  className?: string;
}) {
  return (
    <figure className={cn("flex flex-col gap-3.5", className)}>
      <button
        type="button"
        onClick={onOpen}
        aria-label={`Enlarge ${shot.title}`}
        className={cn(
          "group relative block overflow-hidden rounded-xl border border-border bg-card p-2 text-left",
          PLATE_SHADOW,
          "transition-transform duration-500 ease-[var(--ease-out-quint)] hover:-translate-y-1 focus-visible:-translate-y-1 focus-visible:outline-none focus-visible:ring-4 focus-visible:ring-ring/30",
        )}
      >
        <img
          src={shot.file}
          alt={shot.alt}
          width={shot.width}
          height={shot.height}
          loading="lazy"
          className="w-full rounded-lg"
        />
        <span className="pointer-events-none absolute right-4 top-4 flex items-center gap-1.5 rounded-full border border-border bg-background/90 px-2.5 py-1 font-mono text-[0.65rem] text-muted-foreground opacity-0 transition-opacity duration-300 group-hover:opacity-100 group-focus-visible:opacity-100">
          <Expand className="size-3" />
          enlarge
        </span>
      </button>

      <figcaption className="flex flex-col gap-1">
        <span className="flex items-baseline gap-3">
          <span className="font-mono text-xs tracking-wide text-primary">{shot.fig}</span>
          <span className="font-display text-lg text-foreground sm:text-xl">{shot.title}</span>
        </span>
        <p className="max-w-[60ch] text-sm leading-relaxed text-muted-foreground">{shot.blurb}</p>
      </figcaption>
    </figure>
  );
}
