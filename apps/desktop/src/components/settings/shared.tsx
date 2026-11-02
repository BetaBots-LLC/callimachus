// The vocabulary every Settings section is built from, so the screen reads as one
// surface: a header, groups of rows in a bordered list, and a few small status pieces.
//
// Rows, not cards: a group is one bordered container with the rows divided by hairlines,
// the way system settings panes lay out. Titles sit outside the container.
//
// Toasts (lib/toast.ts) are for outcomes the user can't see in place: a keychain save, a
// background job finishing, an install. A switch that visibly flips gets none.
import type { ReactNode } from "react";
import { CircleCheck, CircleDashed, TriangleAlert } from "lucide-react";
import { cn } from "@/lib/utils";

/** Section title + one-line purpose, at the top of every section. */
export function SectionHeader({
  title,
  description,
  children,
}: {
  title: string;
  description?: ReactNode;
  /** Right-aligned actions (a primary button, a link). */
  children?: ReactNode;
}) {
  return (
    <header className="mb-6 flex items-start justify-between gap-6">
      <div className="min-w-0 space-y-1">
        <h2 className="font-heading text-lg leading-tight font-semibold tracking-tight">{title}</h2>
        {description && (
          <p className="max-w-prose text-[13px] leading-normal text-muted-foreground">
            {description}
          </p>
        )}
      </div>
      {children && <div className="flex shrink-0 items-center gap-2 pt-0.5">{children}</div>}
    </header>
  );
}

/** A titled group of rows. Rows are separated by hairlines inside one bordered container. */
export function SettingsGroup({
  title,
  description,
  children,
  className,
  "aria-label": ariaLabel,
}: {
  title?: string;
  description?: ReactNode;
  children: ReactNode;
  className?: string;
  "aria-label"?: string;
}) {
  return (
    <section aria-label={ariaLabel ?? title} className={cn("space-y-2", className)}>
      {(title || description) && (
        <div className="space-y-0.5 px-0.5">
          {title && <h3 className="text-sm font-semibold">{title}</h3>}
          {description && (
            <p className="text-[13px] leading-normal text-muted-foreground">{description}</p>
          )}
        </div>
      )}
      <div className="divide-y divide-border overflow-hidden rounded-xl border bg-card">
        {children}
      </div>
    </section>
  );
}

/**
 * One row of a group. `control` sits on the trailing edge (a switch, a button, a select);
 * `children` take the full width below the title (inputs, status lists, notes). Pass
 * `htmlFor` with the control's id so clicking the title focuses or toggles it.
 */
export function SettingRow({
  title,
  description,
  control,
  children,
  htmlFor,
  compact = false,
  className,
}: {
  /** Omit it for a row that is only a field grid under its group's title. */
  title?: ReactNode;
  description?: ReactNode;
  control?: ReactNode;
  children?: ReactNode;
  htmlFor?: string;
  /** Tighter padding for lists of many like rows (one line of text and a button each). */
  compact?: boolean;
  className?: string;
}) {
  const titleClass = "text-sm leading-snug font-medium";
  return (
    <div className={cn("flex flex-col gap-3 px-4", compact ? "py-2" : "py-3.5", className)}>
      {(title || description || control) && (
        <div
          className={cn("flex justify-between gap-6", description ? "items-start" : "items-center")}
        >
          <div className="min-w-0 flex-1 space-y-0.5">
            {htmlFor ? (
              <label htmlFor={htmlFor} className={cn(titleClass, "block cursor-default")}>
                {title}
              </label>
            ) : (
              <div className={titleClass}>{title}</div>
            )}
            {description && (
              <p className="text-[13px] leading-normal text-muted-foreground">{description}</p>
            )}
          </div>
          {control && <div className="flex shrink-0 items-center gap-2">{control}</div>}
        </div>
      )}
      {children}
    </div>
  );
}

/** Secondary explanation under a control or at the end of a group. */
export function Note({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <p className={cn("text-[13px] leading-normal text-muted-foreground", className)}>{children}</p>
  );
}

/** An inline failure, announced to screen readers. Pair with a toast for async failures. */
export function InlineError({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <p
      role="alert"
      className={cn("text-[13px] leading-normal break-words text-destructive", className)}
    >
      {children}
    </p>
  );
}

export type StatusState = "ok" | "missing" | "warning";

/** One part of a multi-part status ("skill", "MCP server", …) with a drawn icon. */
export function StatusItem({ state, children }: { state: StatusState; children: ReactNode }) {
  const Icon = state === "ok" ? CircleCheck : state === "warning" ? TriangleAlert : CircleDashed;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 text-[13px]",
        state === "ok" && "text-foreground",
        state === "warning" && "text-amber-700 dark:text-amber-400",
        state === "missing" && "text-muted-foreground",
      )}
    >
      <Icon
        aria-hidden="true"
        className={cn(
          "size-3.5 shrink-0",
          state === "ok" && "text-emerald-600 dark:text-emerald-400",
        )}
      />
      {children}
    </span>
  );
}

export function StatusList({ children }: { children: ReactNode }) {
  return <div className="flex flex-wrap gap-x-4 gap-y-1.5">{children}</div>;
}

/** The small "connected" / "set" pill next to a title. */
export function StatePill({
  children,
  tone = "ok",
}: {
  children: ReactNode;
  tone?: "ok" | "neutral";
}) {
  return (
    <span
      className={cn(
        "inline-flex h-5 items-center rounded-full px-2 text-[11px] font-medium",
        tone === "ok"
          ? "bg-emerald-500/12 text-emerald-700 dark:bg-emerald-400/15 dark:text-emerald-300"
          : "bg-muted text-muted-foreground",
      )}
    >
      {children}
    </span>
  );
}

/** A command the user may want to copy, set apart from prose. */
export function CommandText({ children }: { children: ReactNode }) {
  return (
    <code className="block w-fit max-w-full rounded-md bg-muted px-2 py-1 font-mono text-xs break-all text-foreground select-all">
      {children}
    </code>
  );
}
