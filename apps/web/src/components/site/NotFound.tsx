import { Link } from "@tanstack/react-router";
import { Container } from "./Container";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

// Rendered by the router for any unmatched path (TanStack Start returns HTTP 404).
export function NotFound() {
  return (
    <Container className="grid min-h-[60vh] place-items-center py-24 text-center">
      <div className="flex flex-col items-center gap-5">
        <span className="cat-label text-primary">№ 404 — not in the catalogue</span>
        <h1 className="text-balance text-4xl text-foreground sm:text-5xl">
          This card isn't filed here.
        </h1>
        <p className="max-w-[46ch] text-muted-foreground">
          The page you're after doesn't exist, or it got re-shelved. Head back to the front desk.
        </p>
        <div className="mt-1 flex flex-wrap justify-center gap-3">
          <Link to="/" className={cn(buttonVariants())}>
            Back to home
          </Link>
          <Link to="/download" className={cn(buttonVariants({ variant: "outline" }))}>
            Download
          </Link>
        </div>
      </div>
    </Container>
  );
}
