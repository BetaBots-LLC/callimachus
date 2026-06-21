-- Distillation staleness was keyed off threads.message_count (TOTAL rows, including tool /
-- system output). Agent transcripts grow mostly via tool rows, so a thread flipped "stale"
-- and re-triggered a paid LLM re-distill that produced identical output. Track the count of
-- DISTILLABLE (user/assistant) messages instead, and compare against that.
ALTER TABLE threads ADD COLUMN distillable_count INTEGER NOT NULL DEFAULT 0;

-- Backfill from already-indexed messages.
UPDATE threads SET distillable_count = (
    SELECT COUNT(*) FROM messages
    WHERE messages.thread_id = threads.id AND messages.role IN ('user', 'assistant')
);

-- Keep already-distilled threads from all flipping stale under the new metric: their
-- knowledge_msg_count was stored as the OLD total count, so re-point it at distillable_count.
UPDATE threads SET knowledge_msg_count = distillable_count WHERE knowledge_extracted = 1;
