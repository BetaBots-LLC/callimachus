import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

// Catalogue-style section header: a small numbered/lettered eyebrow, a serif
// title, and an optional lede. Left-aligned by default (centering everything is
// an AI tell).
export function SectionHeading({
  label,
  title,
  intro,
  align = "left",
  className,
}: {
  label: string;
  title: ReactNode;
  intro?: ReactNode;
  align?: "left" | "center";
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex flex-col gap-3",
        align === "center" && "items-center text-center",
        className,
      )}
    >
      <span className="cat-label text-primary">{label}</span>
      <h2 className="max-w-2xl text-balance text-3xl text-foreground sm:text-4xl">{title}</h2>
      {intro && (
        <p
          className={cn(
            "max-w-[60ch] text-base leading-relaxed text-muted-foreground sm:text-lg",
            align === "center" && "mx-auto",
          )}
        >
          {intro}
        </p>
      )}
    </div>
  );
}
