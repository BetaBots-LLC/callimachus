import { cn } from "@/lib/utils";

// A compact scroll mark (currentColor) + the wordmark in the display serif.
export function Logo({ className }: { className?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-2.5", className)}>
      <svg viewBox="0 0 24 24" className="size-7 text-primary" fill="none" aria-hidden="true">
        <title>Callimachus</title>
        <rect x="5" y="3.5" width="14" height="17" rx="2.4" fill="currentColor" opacity="0.16" />
        <path
          d="M7.5 6.5h9M7.5 10h9M7.5 13.5h6"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        />
        <path
          d="M5 5.2a2.5 2.5 0 0 1 2.5-2.5h11a2 2 0 0 1 2 2v0a1.8 1.8 0 0 1-1.8 1.8H7.2"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
        <path
          d="M5 5.2v13.4a2.4 2.4 0 0 0 2.4 2.4h11.1a1.9 1.9 0 0 1-1.9-1.9V6.5"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
      <span className="font-display text-[1.35rem] leading-none tracking-tight text-foreground">
        Callimachus
      </span>
    </span>
  );
}
