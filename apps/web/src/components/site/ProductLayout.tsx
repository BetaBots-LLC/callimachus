import type { ReactNode } from "react";
import { Container } from "./Container";

export function ProductLayout({
  no,
  kicker,
  title,
  description,
  cta,
  children,
}: {
  no: string;
  kicker: string;
  title: string;
  description: ReactNode;
  cta?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Container className="py-16 sm:py-20">
      <header className="max-w-2xl">
        <span className="cat-label text-primary">
          № {no} — {kicker}
        </span>
        <h1 className="mt-3 text-balance text-4xl text-foreground sm:text-5xl">{title}</h1>
        <p className="mt-4 text-lg leading-relaxed text-muted-foreground">{description}</p>
        {cta && <div className="mt-8 flex flex-wrap items-center gap-3">{cta}</div>}
      </header>
      <div className="mt-16">{children}</div>
    </Container>
  );
}
