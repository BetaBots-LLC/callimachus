-- 0028 — Claude Code external ids always use forward slashes.
--
-- The Claude indexer built external_id from the transcript path relative to
-- ~/.claude/projects with the platform separator, so on Windows ids looked like
-- `slug\uuid\subagents\agent-x.jsonl`. Subagent detection (`/subagents/`) and the
-- project-slug fallback both split on '/', so every subagent was indexed as a top-level
-- thread and threads without a cwd got a garbage project. The indexer now always emits
-- '/'; this rewrites existing rows to match so a re-index updates them in place (keeping
-- stars, tags and distilled facts) instead of creating duplicates.
--
-- Everything below only touches ids containing a backslash, which in practice only
-- exist on Windows installs. macOS/Linux indexes are unaffected.

-- If a forward-slash twin already exists, the backslash row is a stale duplicate of the
-- same transcript: drop it (cascades to its messages, vectors and facts).
DELETE FROM threads
WHERE source_id = (SELECT id FROM sources WHERE kind = 'claude_code')
  AND instr(external_id, '\') > 0
  AND EXISTS (
      SELECT 1 FROM threads t2
      WHERE t2.source_id = threads.source_id
        AND t2.external_id = replace(threads.external_id, '\', '/')
  );

UPDATE threads
SET external_id = replace(external_id, '\', '/'),
    is_subagent = (instr(replace(external_id, '\', '/'), '/subagents/') > 0)
WHERE source_id = (SELECT id FROM sources WHERE kind = 'claude_code')
  AND instr(external_id, '\') > 0;

-- Forget the file cursors for those transcripts (Windows paths contain '\'), so the next
-- scan re-parses them and fixes the slug-derived project path for threads without a cwd.
DELETE FROM index_state
WHERE source_kind = 'claude_code' AND instr(path, '\') > 0;
