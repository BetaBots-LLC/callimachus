import { cn } from "@/lib/utils";

// A terminal snippet. `lines` render with a faint $ prompt; comment lines (starting
// with #) read as muted. No copy button by default — keep it calm and legible.
export function CommandBlock({
  lines,
  label,
  className,
}: {
  lines: string[];
  label?: string;
  className?: string;
}) {
  return (
    <div className={cn("overflow-hidden rounded-md border border-border", className)}>
      {label && (
        <div className="border-b border-border bg-card px-4 py-2">
          <span className="cat-label">{label}</span>
        </div>
      )}
      <pre className="overflow-x-auto bg-[oklch(0.155_0.011_56)] px-4 py-3.5 font-mono text-sm leading-relaxed">
        <code>
          {lines.map((line) => {
            const comment = line.trimStart().startsWith("#");
            return (
              <span key={line} className="block">
                {comment ? (
                  <span className="text-muted-foreground">{line}</span>
                ) : (
                  <>
                    <span className="select-none text-primary/70">$ </span>
                    <span className="text-foreground/90">{line}</span>
                  </>
                )}
              </span>
            );
          })}
        </code>
      </pre>
    </div>
  );
}
