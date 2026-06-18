import type { LegalDoc } from "@/content/legal";
import { Container } from "./Container";

export function LegalPage({ doc }: { doc: LegalDoc }) {
  return (
    <Container className="py-16 sm:py-20">
      <article className="mx-auto max-w-[68ch]">
        <span className="cat-label text-primary">№ — on the record</span>
        <h1 className="mt-3 text-4xl text-foreground sm:text-5xl">{doc.title}</h1>
        <p className="mt-2 font-mono text-xs text-muted-foreground">Last updated {doc.updated}</p>

        <p className="mt-8 border-y border-border py-5 text-lg leading-relaxed text-foreground">
          {doc.summary}
        </p>

        <div className="mt-10 flex flex-col gap-9">
          {doc.sections.map((s) => (
            <section key={s.heading}>
              <h2 className="text-xl text-foreground">{s.heading}</h2>
              <div className="mt-3 flex flex-col gap-3">
                {s.body.map((p) => (
                  <p key={p} className="leading-relaxed text-muted-foreground">
                    {p}
                  </p>
                ))}
              </div>
            </section>
          ))}
        </div>

        <p className="mt-12 rounded-md border border-border bg-card p-4 font-mono text-xs leading-relaxed text-muted-foreground">
          Note: this is a plain-language template for an open-source project, not legal advice. Have
          it reviewed by counsel before relying on it.
        </p>
      </article>
    </Container>
  );
}
