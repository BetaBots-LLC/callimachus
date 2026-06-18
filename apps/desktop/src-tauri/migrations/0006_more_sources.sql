-- More indexable CLI/agent sources.
--   qwen     — Qwen Code     ~/.qwen/tmp/<hash>/chats/*.jsonl
--   goose    — Block Goose   ~/.local/share/goose/sessions/sessions.db
--   opencode — OpenCode      ~/.local/share/opencode/storage/{session,message,part}/*.json
--   continue — Continue CLI  ~/.continue/sessions/*.json
--   cline    — Cline (VS Code, index-only)  <editor>/globalStorage/saoudrizwan.claude-dev/tasks/
INSERT OR IGNORE INTO sources (kind) VALUES
    ('qwen'), ('goose'), ('opencode'), ('continue'), ('cline');
