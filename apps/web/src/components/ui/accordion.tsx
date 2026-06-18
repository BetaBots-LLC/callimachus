import { Accordion } from "@base-ui/react/accordion";
import { Plus } from "lucide-react";

// FAQ list styled as ruled catalogue entries. Base UI drives the open/close;
// the panel animates on the height CSS var it exposes.
export function FaqAccordion({ items }: { items: { q: string; a: string }[] }) {
  return (
    <Accordion.Root className="border-t border-border" multiple={false}>
      {items.map((it) => (
        <Accordion.Item key={it.q} className="border-b border-border">
          <Accordion.Header className="m-0">
            <Accordion.Trigger className="group/trigger flex w-full cursor-pointer items-center justify-between gap-6 py-5 text-left outline-none focus-visible:text-link">
              <span className="font-display text-lg leading-snug text-foreground sm:text-xl">
                {it.q}
              </span>
              <Plus className="size-5 shrink-0 text-muted-foreground transition-transform duration-200 ease-[var(--ease-out-quint)] group-aria-expanded/trigger:rotate-45 group-aria-expanded/trigger:text-link" />
            </Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel className="h-[var(--accordion-panel-height)] overflow-hidden text-muted-foreground transition-[height] duration-200 ease-[var(--ease-out-quint)] data-[ending-style]:h-0 data-[starting-style]:h-0">
            <p className="max-w-[68ch] pr-10 pb-6 leading-relaxed">{it.a}</p>
          </Accordion.Panel>
        </Accordion.Item>
      ))}
    </Accordion.Root>
  );
}
