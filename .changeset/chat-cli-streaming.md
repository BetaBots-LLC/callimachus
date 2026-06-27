---
"callimachus": patch
---

Stream the Claude Code CLI chat engine token-by-token. The keyless `claude` backend now runs with `--output-format stream-json --include-partial-messages` and forwards each text/thinking delta as it arrives, so replies stream in smoothly instead of popping in all at once when the response completes. (Codex CLI still emits its reply in one chunk.)

Define the scrollbar utilities the shadcn message-scroller viewport relies on (`scrollbar-gutter-stable`, `scrollbar-thin`, `scrollbar-none`, and the `data-autoscrolling` variant), which aren't shipped in `shadcn/tailwind.css`. Reserving the scrollbar gutter stops the chat column from shifting horizontally as a reply streams in, and the scrollbar no longer flickers during autoscroll.
