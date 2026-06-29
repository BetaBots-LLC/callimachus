-- VS Code-native / GitHub Copilot chat as an indexable source. Stored by the editor
-- itself (Code / Cursor / VSCodium / Windsurf / Insiders) under
--   <editor>/User/workspaceStorage/<hash>/chatSessions/<uuid>.jsonl  and
--   <editor>/User/globalStorage/emptyWindowChatSessions/<uuid>.jsonl
-- as one JSON object {kind, v:{requests[]}}. The per-turn modelId is captured into the
-- existing messages.model column (added in 0022), so each assistant turn keeps its model.
INSERT OR IGNORE INTO sources (kind) VALUES ('copilot');
