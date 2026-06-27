---
"callimachus": patch
---

Rebuild the in-app chat UI on the new shadcn conversation primitives (Bubble, Message, Marker, Attachment, Message Scroller) instead of the bespoke layout. User turns render as muted bubbles, assistant turns as ghost-bubble markdown, injected context as a compact Attachment chip, and system notes as inline markers. The message list now uses Message Scroller for autoscroll / stick-to-bottom (replacing `use-stick-to-bottom`), with a scroll-to-latest button.
