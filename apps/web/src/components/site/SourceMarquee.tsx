import { SOURCES } from "@/lib/site";

// An endless card-catalogue drawer: the indexed agents scroll past, numbered.
// Duplicated once so the -50% translate loops seamlessly. Pauses on hover and
// for reduced-motion (handled in CSS).
export function SourceMarquee() {
  const items = [...SOURCES, ...SOURCES].map((name, i) => ({
    name,
    key: `${i}-${name}`,
    no: String((i % SOURCES.length) + 1).padStart(3, "0"),
  }));
  return (
    <div
      className="group relative overflow-hidden py-2"
      style={{
        maskImage: "linear-gradient(to right, transparent, black 6%, black 94%, transparent)",
        WebkitMaskImage: "linear-gradient(to right, transparent, black 6%, black 94%, transparent)",
      }}
    >
      <ul className="marquee-track flex w-max items-center gap-3 group-hover:[animation-play-state:paused]">
        {items.map((it) => (
          <li
            key={it.key}
            className="flex shrink-0 items-center gap-2.5 rounded-md border border-border bg-card px-4 py-2"
          >
            <span className="font-mono text-[0.65rem] text-primary">№ {it.no}</span>
            <span className="text-sm whitespace-nowrap text-foreground">{it.name}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
