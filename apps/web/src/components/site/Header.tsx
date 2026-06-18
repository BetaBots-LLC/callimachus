import { Link } from "@tanstack/react-router";
import { Logo } from "./Logo";
import { GithubMark } from "./icons";
import { Container } from "./Container";
import { GITHUB_URL, NAV } from "@/lib/site";
import { buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export function Header() {
  return (
    <header className="sticky top-0 z-50 border-b border-border bg-background/95">
      <Container className="flex h-16 items-center justify-between gap-6">
        <Link
          to="/"
          aria-label="Callimachus home"
          className="rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
        >
          <Logo />
        </Link>

        <nav className="hidden items-center gap-7 md:flex">
          {NAV.map((n) => (
            <Link
              key={n.href}
              to={n.href}
              className="text-sm text-muted-foreground transition-colors hover:text-foreground [&.active]:text-foreground"
            >
              {n.label}
            </Link>
          ))}
        </nav>

        <div className="flex items-center gap-1.5">
          <a
            href={GITHUB_URL}
            target="_blank"
            rel="noreferrer"
            aria-label="Callimachus on GitHub"
            className="grid size-9 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-card hover:text-foreground"
          >
            <GithubMark className="size-[18px]" />
          </a>
          <Link to="/download" className={cn(buttonVariants({ size: "sm" }))}>
            Download
          </Link>
        </div>
      </Container>
    </header>
  );
}
