export const FAQ: { q: string; a: string }[] = [
  {
    q: "Does my data leave my machine?",
    a: "Your history doesn't. Callimachus reads the conversation logs your agents already write locally and builds a SQLite index on disk. Search, embeddings, and the index all stay on your computer, and there is no account and no cloud sync. Conversation content leaves only when you use the in-app chat, distillation, or Ask with a cloud AI provider you choose (local Ollama keeps even that on your machine). Beyond that, the app checks GitHub for updates, downloads its embedding model once, and sends anonymous usage analytics (event names, the app version and platform, and a few properties from fixed lists such as rounded counts; never your content) only if you choose to allow them.",
  },
  {
    q: "Which AI coding agents does it index?",
    a: "Twelve today: Claude Code, Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, Kilo Code, and Copilot Chat in VS Code-family editors, plus the in-app chat. A background watcher keeps the index current as you work, and you can reindex any source on demand.",
  },
  {
    q: "How is the search better than grep?",
    a: "It's hybrid. Keyword search (SQLite FTS5) is fused with on-device semantic search (a small embedding model running locally) so you can find a thread by what it was about, not just the exact words you typed. Filter by source, project, recency, starred, tag, or subagents — or type file:embed/mod.rs to find every thread that touched a code path.",
  },
  {
    q: "What do I actually need to run it?",
    a: "The desktop app on macOS (Apple Silicon), Windows, or Linux (glibc 2.38 or newer, such as Ubuntu 24.04+ or Debian 13+). That's it: it builds the index and can install the cal CLI. The editor extension and MCP server both read the same local index, so run the app once and everything else just works.",
  },
  {
    q: "Can it summarize decisions or answer questions about my history?",
    a: "Yes. TODOs are extracted for free with no key. Turn on distillation (local Ollama, keyless, or a cloud API key) and Callimachus distills each thread into a summary plus key decisions and gotchas, then recalls them semantically across every thread. The Ask tab goes further: it answers a question over your own history with a synthesized, cited response that links [thread N] back to the sources it used.",
  },
  {
    q: "How do I get a past thread back into my agent?",
    a: "Open any thread and copy its packed context, insert it into your editor, export it to Obsidian, or let the MCP server hand it to an agent on demand. The point is to make your own history reusable, not just readable.",
  },
  {
    q: "Is it free?",
    a: "Yes — Callimachus is open source under AGPL-3.0. A commercial license is available if AGPL doesn't fit your use (closed-source redistribution, proprietary SaaS). See pricing for details.",
  },
];
