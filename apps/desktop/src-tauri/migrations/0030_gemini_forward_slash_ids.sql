-- 0030: Gemini CLI external ids always use forward slashes (the 0028 fix, for Gemini).
--
-- The Gemini indexer keyed threads by the transcript path relative to ~/.gemini/tmp with
-- the platform separator, so Windows ids held backslashes and the `chats/` segment check
-- (which splits on '/') never flagged subagent transcripts. The indexer now emits '/';
-- rewriting existing rows in place keeps their stars, tags and distilled facts instead of
-- re-importing every Gemini thread as a duplicate. Only ids containing a backslash are
-- touched, which in practice exist only on Windows installs.

-- A forward-slash twin means the backslash row is a stale duplicate: drop it.
DELETE FROM threads
WHERE source_id = (SELECT id FROM sources WHERE kind = 'gemini')
  AND instr(external_id, '\') > 0
  AND EXISTS (
      SELECT 1 FROM threads t2
      WHERE t2.source_id = threads.source_id
        AND t2.external_id = replace(threads.external_id, '\', '/')
  );

UPDATE threads
SET external_id = replace(external_id, '\', '/')
WHERE source_id = (SELECT id FROM sources WHERE kind = 'gemini')
  AND instr(external_id, '\') > 0;

-- Forget those transcripts' file cursors (Windows paths contain '\'), so the next scan
-- re-parses them under the fixed id and recomputes the subagent flag.
DELETE FROM index_state
WHERE source_kind = 'gemini' AND instr(path, '\') > 0;
