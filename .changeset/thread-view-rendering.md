---
"callimachus": patch
---

Thread view: rich rendering + chat-style scrolling.

- Indexed messages now render as **Markdown** with **syntax-highlighted** code blocks (previously plain text); tool calls and JSON results are pretty-printed.
- Threads **open at the newest message and scroll up for history**, like a chat — on the same virtualized list.
- Big scroll-performance win on long threads: message HTML is parsed once and cached, so scrolling back through hundreds of messages no longer re-parses + re-highlights each one. Thin, inset scrollbars so a code block's bar no longer overlaps the line beneath it.
- The thread header is decluttered into Knowledge · ★ · Resume · a "more" menu.
