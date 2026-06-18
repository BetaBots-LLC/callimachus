export const FAQ: { q: string; a: string }[] = [
  {
    q: "Does my data leave my machine?",
    a: "No. Callimachus reads the conversation logs your agents already write locally and builds a SQLite index on disk. Search, embeddings, and the index all stay on your computer. There is no account, no cloud sync, and no telemetry. The only time anything leaves is if you explicitly use the in-app chat with your own provider key.",
  },
  {
    q: "Which AI coding agents does it index?",
    a: "Eleven today: Claude Code, Codex, Cursor, Gemini CLI, Qwen Code, Goose, OpenCode, Continue, Cline, Roo Code, and Kilo Code — plus the in-app chat. A background watcher keeps the index current as you work, and you can reindex any source on demand.",
  },
  {
    q: "How is the search better than grep?",
    a: "It's hybrid. Keyword search (SQLite FTS5) is fused with on-device semantic search (a small embedding model running locally) so you can find a thread by what it was about, not just the exact words you typed. Filter by source, project, or recency.",
  },
  {
    q: "What do I actually need to run it?",
    a: "The desktop app on macOS, Windows, or Linux — that's it; it bundles the index and the cal CLI. The editor extension and MCP server both read the same local index, so run the app once and everything else just works.",
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
