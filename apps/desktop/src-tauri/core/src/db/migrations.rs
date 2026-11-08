//! Schema migrations, embedded at compile time and run on startup.

use rusqlite_migration::{Migrations, M};
use std::sync::LazyLock;

pub static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        // 0001 — canonical store: sources, threads, messages, FTS5, index_state, providers.
        M::up(include_str!("../../../migrations/0001_init.sql")),
        // 0002 — is_subagent flag on threads.
        M::up(include_str!("../../../migrations/0002_subagent.sql")),
        // 0003 — embeddings table for semantic search.
        M::up(include_str!("../../../migrations/0003_embeddings.sql")),
        // 0004 — sqlite-vec vec0 chunk index (replaces the embeddings BLOB table).
        M::up(include_str!("../../../migrations/0004_vec0.sql")),
        // 0005 — Gemini CLI as an indexable source.
        M::up(include_str!("../../../migrations/0005_gemini.sql")),
        // 0006 — Qwen Code, Goose, OpenCode, Continue, Cline sources.
        M::up(include_str!("../../../migrations/0006_more_sources.sql")),
        // 0007 — Roo Code, Kilo Code (Cline-architecture forks).
        M::up(include_str!("../../../migrations/0007_cline_forks.sql")),
        // 0008 — messages.embedded flag + partial index (fast incremental embedding).
        M::up(include_str!("../../../migrations/0008_embed_flag.sql")),
        // 0009 — precomputed threads.bytes (fast cleanup list).
        M::up(include_str!("../../../migrations/0009_thread_bytes.sql")),
        // 0010 — stars + free-form tags ("collections").
        M::up(include_str!("../../../migrations/0010_stars_tags.sql")),
        // 0011 — distilled knowledge layer (facts: todos now, decisions/gotchas later).
        M::up(include_str!("../../../migrations/0011_knowledge.sql")),
        // 0012 — opt-in LLM distillation tier: extraction state, fact vectors, config.
        M::up(include_str!("../../../migrations/0012_knowledge_llm.sql")),
        // 0013 — file-path mentions for code-aware search.
        M::up(include_str!("../../../migrations/0013_file_mentions.sql")),
        // 0014 — perf: composite list indexes, facts(thread_id,kind), trigram path FTS.
        M::up(include_str!("../../../migrations/0014_perf.sql")),
        // 0015 — fact curation: pin / edit / hide distilled facts.
        M::up(include_str!("../../../migrations/0015_fact_curation.sql")),
        // 0016 — canonical project key for stable per-project grouping.
        M::up(include_str!("../../../migrations/0016_project_key.sql")),
        // 0017 — index messages.ts for the Coach activity heatmap range scan.
        M::up(include_str!("../../../migrations/0017_messages_ts.sql")),
        // 0018 — track distillable (user/assistant) message count for distill staleness.
        M::up(include_str!(
            "../../../migrations/0018_distillable_count.sql"
        )),
        // 0019 — agent session snapshots: resumable thread checkpoints for cross-agent handoff.
        M::up(include_str!("../../../migrations/0019_snapshots.sql")),
        // 0020 — ADR-style decision rationale ("why") for the contradiction guard.
        M::up(include_str!(
            "../../../migrations/0020_decision_rationale.sql"
        )),
        // 0021 — git linkage: commits a thread produced (file-overlap within its time window).
        M::up(include_str!("../../../migrations/0021_thread_commits.sql")),
        // 0022 — per-message token usage + model, for the cost/spend layer.
        M::up(include_str!("../../../migrations/0022_message_usage.sql")),
        // 0023 — index thread_commits by sha for the commits_by_sha inverse lookup (cal audit-pr).
        M::up(include_str!(
            "../../../migrations/0023_thread_commits_sha_idx.sql"
        )),
        // 0024 — VS Code-native / GitHub Copilot chat as a source (per-turn model).
        M::up(include_str!("../../../migrations/0024_copilot.sql")),
        // 0025 — chunk-rowid -> message map so a message delete removes its vectors by
        // point lookup instead of a full vec0 scan per deleted message.
        M::up(include_str!("../../../migrations/0025_vec_chunk_owner.sql")),
        // 0026 — covering indexes for the embedding-progress / stats / spend aggregates.
        M::up(include_str!(
            "../../../migrations/0026_embed_count_index.sql"
        )),
        // 0027 — tombstones so deleted threads are not re-imported by the next scan.
        M::up(include_str!("../../../migrations/0027_deleted_threads.sql")),
        // 0028 — normalize Windows-style Claude Code external ids to forward slashes.
        M::up(include_str!(
            "../../../migrations/0028_claude_forward_slash_ids.sql"
        )),
        // 0029: drop tool-call vectors and re-queue prose marked embedded without vectors.
        M::up(include_str!(
            "../../../migrations/0029_embed_prose_only.sql"
        )),
        // 0030: normalize Windows-style Gemini external ids to forward slashes.
        M::up(include_str!(
            "../../../migrations/0030_gemini_forward_slash_ids.sql"
        )),
        // 0031: covering indexes for the FTS join, Coach heatmap and commit linker; drops
        // the redundant (thread_id) index and 0008's unused partial index.
        M::up(include_str!(
            "../../../migrations/0031_messages_cover_indexes.sql"
        )),
        // 0032: facts.origin_ref, the stable key of a fact imported from an agent's own
        // files (Claude Code auto memory, compaction summaries), replaced by that key.
        M::up(include_str!("../../../migrations/0032_fact_origin_ref.sql")),
        // 0033: Copilot CLI as a source (one row in `sources`).
        M::up(include_str!("../../../migrations/0033_copilot_cli.sql")),
        // 0034: fact identity (facts.uid, facts.source_seq) and the graph tables: projects
        // (portable ids), file_edits (per tool call), issue_clusters; plus a
        // threads(last_indexed_at) index for the watcher's commit linking.
        M::up(include_str!(
            "../../../migrations/0034_identity_and_graph.sql"
        )),
    ])
});

/// Migrations (by number, so 32 is 0032) that never scan `messages` or another big table:
/// column adds and indexes over small tables. When every pending migration is listed here,
/// `db::migrate` skips its sequential pre-read of the whole file, which costs about 1.2 s
/// warm (several seconds cold) on a 5.1 GB index, far more than these migrations themselves
/// (0032 is one ALTER ADD COLUMN and a partial index over `facts`; 0033 one row in `sources`;
/// 0034 backfills `facts` (2k rows), creates empty tables and indexes `threads`: opening a
/// copy of that index at 0032 ran 0033 and 0034 in 0.4 s cold). Add a migration here only when
/// it touches no large table.
pub const SCAN_FREE_MIGRATIONS: &[usize] = &[32, 33, 34];

/// Whether every migration after `applied` (the index's `user_version`) up to `latest` is in
/// [`SCAN_FREE_MIGRATIONS`].
pub fn pending_are_scan_free(applied: usize, latest: usize) -> bool {
    (applied + 1..=latest).all(|n| SCAN_FREE_MIGRATIONS.contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn scan_free_pending_skips_the_preread() {
        assert!(pending_are_scan_free(31, 32), "0032 alone is cheap");
        assert!(pending_are_scan_free(31, 33), "so is 0033");
        assert!(pending_are_scan_free(31, 34), "and 0034");
        assert!(
            !pending_are_scan_free(30, 32),
            "0031 builds indexes on messages"
        );
        assert!(pending_are_scan_free(32, 32), "nothing pending");
        let latest = MIGRATIONS
            .pending_migrations(&rusqlite::Connection::open_in_memory().unwrap())
            .unwrap() as usize;
        assert!(
            SCAN_FREE_MIGRATIONS
                .iter()
                .all(|n| (1..=latest).contains(n)),
            "every listed migration exists"
        );
    }

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "calli_mig_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// A fresh install must apply every embedded migration cleanly and land on an
    /// applied version. `db::open` registers the vec0 extension (migration 0004 needs
    /// it) and runs `to_latest`, so a successful open *is* the real first-run path.
    /// Then `validate()` re-applies all migrations on its own in-memory DB — which
    /// turns a malformed or out-of-order migration into a CI failure, not a user's
    /// broken first launch.
    #[test]
    fn fresh_db_migrates_then_validates() {
        let p = temp_path();
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).expect("fresh DB should migrate cleanly");
        assert!(
            matches!(
                MIGRATIONS.current_version(&conn).unwrap(),
                rusqlite_migration::SchemaVersion::Inside(_)
            ),
            "a migrated DB should report an applied schema version"
        );
        // vec0 is now registered process-wide, so validate()'s in-memory apply works.
        MIGRATIONS
            .validate()
            .expect("embedded migrations should validate");
    }

    // ---- 0025: vector cleanup by chunk rowid -------------------------------------------

    use rusqlite::{params, Connection};

    fn fresh_db() -> Connection {
        let p = temp_path();
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        crate::db::open(&p).expect("open + migrate")
    }

    /// Open a raw connection at `version` (vec0 registered, FKs on), for upgrade tests.
    fn db_at_version(version: usize) -> Connection {
        // db::open registers the vec0 auto-extension for every later connection.
        let _ = fresh_db();
        let p = temp_path();
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let mut conn = Connection::open(&p).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        MIGRATIONS.to_version(&mut conn, version).unwrap();
        conn
    }

    fn vec_blob(hot: usize) -> Vec<u8> {
        let mut v = vec![0.0_f32; 384];
        v[hot % 384] = 1.0;
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.join(" | ")
    }

    fn thread_with_messages(conn: &Connection, embedded: &[bool]) -> (i64, Vec<i64>) {
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, ?1, 't')",
            [format!("t{}", N.fetch_add(1, Ordering::Relaxed))],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        let mut ids = Vec::new();
        for (seq, e) in embedded.iter().enumerate() {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, embedded)
                 VALUES (?1, ?2, 'user', 'hello', ?3)",
                params![tid, seq as i64, *e as i64],
            )
            .unwrap();
            ids.push(conn.last_insert_rowid());
        }
        (tid, ids)
    }

    /// The embedder's contract: insert the vector, then record its owner with the rowid
    /// vec0 just assigned (a vec0 insert sets last_insert_rowid like any table).
    fn store_chunk(conn: &Connection, message_id: i64, hot: usize) -> i64 {
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            params![message_id, vec_blob(hot)],
        )
        .unwrap();
        let rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO vec_chunk_owner (chunk_rowid, message_id) VALUES (?1, ?2)",
            params![rowid, message_id],
        )
        .unwrap();
        rowid
    }

    #[test]
    fn message_delete_removes_its_vectors_through_the_owner_map() {
        let conn = fresh_db();
        let (tid, ids) = thread_with_messages(&conn, &[true, true, false]);
        let a1 = store_chunk(&conn, ids[0], 0);
        let _a2 = store_chunk(&conn, ids[0], 1);
        let b1 = store_chunk(&conn, ids[1], 2);

        // last_insert_rowid() after a vec0 insert really is that vector's rowid.
        let owner: i64 = conn
            .query_row(
                "SELECT message_id FROM vec_chunks WHERE rowid = ?1",
                [a1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, ids[0]);

        // Deleting one message removes exactly its two vectors and owner rows.
        conn.execute("DELETE FROM messages WHERE id = ?1", [ids[0]])
            .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunks"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunk_owner"), 1);
        let left: i64 = conn
            .query_row("SELECT rowid FROM vec_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, b1, "the other message's vector survives");

        // Deleting the thread cascades through messages to the remaining vector.
        conn.execute("DELETE FROM threads WHERE id = ?1", [tid])
            .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunks"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunk_owner"), 0);
    }

    #[test]
    fn vector_cleanup_never_plans_a_full_vec0_scan() {
        let conn = fresh_db();
        // vec0 plan ids live in the idxStr: '1' = full scan, '2' = point (rowid =).
        let point = plan(&conn, "DELETE FROM vec_chunks WHERE rowid = 7");
        assert!(
            point.contains(":2"),
            "rowid delete must be a point lookup: {point}"
        );
        let scan = plan(&conn, "DELETE FROM vec_chunks WHERE message_id = 7");
        assert!(
            scan.contains(":1"),
            "metadata filter is a full scan: {scan}"
        );

        let owner = plan(&conn, "DELETE FROM vec_chunk_owner WHERE message_id = 7");
        assert!(
            owner.contains("idx_vec_chunk_owner_message"),
            "owner delete must use its index: {owner}"
        );

        // Regression guard: the hot-path triggers never filter vec_chunks on a non-rowid
        // column (only the unmapped fallback may, see the step-count test below).
        let bodies: Vec<String> = conn
            .prepare(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'trigger' AND name IN ('messages_vec_ad', 'vec_chunk_owner_ad')",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(bodies.len(), 2);
        for sql in &bodies {
            assert!(
                !sql.contains("vec_chunks WHERE message_id"),
                "trigger still full-scans: {sql}"
            );
        }
    }

    /// The real cost guard: deleting a mapped message must not scan the vector table, so
    /// its VM step count (trigger programs included) stays flat as vectors pile up. This
    /// also pins the trigger order the unmapped fallback relies on. A vector without an
    /// owner row is still deleted, the slow way.
    #[test]
    fn mapped_deletes_stay_flat_and_unmapped_vectors_still_go() {
        use rusqlite::StatementStatus;
        let conn = fresh_db();
        let (_tid, ids) = thread_with_messages(&conn, &[true; 4]);
        for (i, id) in ids.iter().enumerate() {
            store_chunk(&conn, *id, i);
        }
        let delete_steps = |id: i64| -> i32 {
            let mut st = conn.prepare("DELETE FROM messages WHERE id = ?1").unwrap();
            st.execute([id]).unwrap();
            st.get_status(StatementStatus::VmStep)
        };
        let before = delete_steps(ids[0]);
        // Thousands more vectors elsewhere.
        let (_t2, more) = thread_with_messages(&conn, &[true; 2000]);
        for (i, id) in more.iter().enumerate() {
            store_chunk(&conn, *id, i);
        }
        let after = delete_steps(ids[1]);
        assert_eq!(before, after, "a mapped delete must not scan vec_chunks");

        // A message whose vector was stored without an owner row (an older embedder).
        let (_t3, unmapped) = thread_with_messages(&conn, &[true]);
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            params![unmapped[0], vec_blob(9)],
        )
        .unwrap();
        let total = count(&conn, "SELECT COUNT(*) FROM vec_chunks");
        delete_steps(unmapped[0]);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM vec_chunks"),
            total - 1,
            "the fallback trigger still removes unmapped vectors"
        );
    }

    #[test]
    fn upgrade_backfills_owner_rows_for_existing_vectors() {
        let mut conn = db_at_version(24);
        let (_tid, ids) = thread_with_messages(&conn, &[true, true]);
        for (i, id) in ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
                params![id, vec_blob(i)],
            )
            .unwrap();
        }
        MIGRATIONS.to_latest(&mut conn).unwrap();

        assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_chunk_owner"), 2);
        conn.execute("DELETE FROM messages WHERE id = ?1", [ids[0]])
            .unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM vec_chunks"),
            1,
            "pre-existing vectors are cleaned up via the backfilled map"
        );
    }

    // ---- 0026: covering index for the progress / stats counts --------------------------

    #[test]
    fn embed_counts_are_served_by_the_covering_index() {
        let conn = fresh_db();
        for sql in [
            "SELECT COUNT(*) FROM messages m WHERE m.embedded = 1
               AND m.role IN ('user','assistant') AND m.tool_name IS NULL",
            "SELECT COUNT(*) FROM messages m
               WHERE m.role IN ('user','assistant') AND m.tool_name IS NULL",
            "SELECT role, COUNT(*) FROM messages GROUP BY role",
        ] {
            let p = plan(&conn, sql);
            assert!(
                p.contains("COVERING INDEX idx_messages_embed_counts"),
                "{sql} -> {p}"
            );
        }
    }

    #[test]
    fn pending_embed_queue_seeks_the_covering_index() {
        let conn = fresh_db();
        let p = plan(
            &conn,
            "SELECT m.id, m.text FROM messages m
               WHERE m.embedded = 0 AND m.role IN ('user','assistant') AND m.tool_name IS NULL
               LIMIT 64",
        );
        // A SEARCH (not a SCAN) keeps each batch O(batch) however many never-embedded
        // tool rows pile up.
        assert!(p.contains("SEARCH"), "{p}");
        assert!(!p.contains("SCAN m"), "{p}");
        // ...through 0026's covering index, which is what let 0031 drop 0008's partial one.
        assert!(p.contains("idx_messages_embed_counts"), "{p}");
    }

    // ---- 0031: messages index swap ----------------------------------------------------

    fn index_names(conn: &Connection) -> Vec<String> {
        conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages'",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    #[test]
    fn upgrade_swaps_the_message_indexes() {
        let mut conn = db_at_version(30);
        let (tid, _) = thread_with_messages(&conn, &[false, true, false]);
        let before = index_names(&conn);
        assert!(before.iter().any(|n| n == "idx_messages_thread"));
        assert!(before.iter().any(|n| n == "idx_messages_pending_embed"));

        MIGRATIONS.to_latest(&mut conn).unwrap();

        let after = index_names(&conn);
        for gone in ["idx_messages_thread", "idx_messages_pending_embed"] {
            assert!(
                !after.iter().any(|n| n == gone),
                "{gone} still there: {after:?}"
            );
        }
        for new in ["idx_messages_thread_ts", "idx_messages_cover"] {
            assert!(after.iter().any(|n| n == new), "{new} missing: {after:?}");
        }
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        // The thread's rows are still found by thread, the way the viewer and the FK
        // cascade look them up.
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE thread_id = ?1",
                [tid],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            3
        );
    }

    /// Dropping the (thread_id) index leaves every per-thread lookup on an index: the
    /// viewer's ordered read on the UNIQUE (thread_id, seq) autoindex, and a thread delete's
    /// cascade still a SEARCH, never a scan of the messages table.
    #[test]
    fn per_thread_reads_keep_an_index_without_idx_messages_thread() {
        let conn = fresh_db();
        let detail = plan(
            &conn,
            "SELECT id, role, text, tool_name, ts, model FROM messages
             WHERE thread_id = 1 ORDER BY seq",
        );
        assert!(
            detail.contains("USING INDEX sqlite_autoindex_messages_1 (thread_id=?)"),
            "{detail}"
        );
        assert!(!detail.contains("TEMP B-TREE"), "{detail}");
        let delete = plan(&conn, "DELETE FROM messages WHERE thread_id = 1");
        assert!(delete.contains("SEARCH messages USING"), "{delete}");
    }

    // ---- 0029: prose-only vectors + repair -----------------------------------------------

    #[test]
    fn upgrade_drops_tool_vectors_and_requeues_unvectored_prose() {
        let mut conn = db_at_version(28);
        let (tid, ids) = thread_with_messages(&conn, &[true, true]);
        // ids[0]: prose with a vector (kept). ids[1]: prose marked embedded with NO vector
        // (the old load-failure path): re-queued.
        let kept = store_chunk(&conn, ids[0], 0);
        // A tool-call row that an older build embedded: its vector goes.
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, tool_name, embedded)
             VALUES (?1, 9, 'assistant', '{\"cmd\":\"ls\"}', 'Bash', 1)",
            [tid],
        )
        .unwrap();
        let tool_msg = conn.last_insert_rowid();
        store_chunk(&conn, tool_msg, 1);
        // ...and one written without an owner row (pre-0025 build after the backfill).
        conn.execute(
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
            params![tool_msg, vec_blob(2)],
        )
        .unwrap();

        MIGRATIONS.to_latest(&mut conn).unwrap();

        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM vec_chunks"),
            1,
            "only the prose vector survives"
        );
        let survivor: i64 = conn
            .query_row("SELECT rowid FROM vec_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(survivor, kept);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM vec_chunk_owner"),
            1,
            "no owner row is left pointing at a deleted vector"
        );
        let embedded = |id: i64| -> i64 {
            conn.query_row("SELECT embedded FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(embedded(ids[0]), 1, "a vectored message stays done");
        assert_eq!(embedded(ids[1]), 0, "prose without a vector is re-queued");
        assert_eq!(
            embedded(tool_msg),
            0,
            "a tool row loses its flag along with its vectors"
        );
    }

    /// Older builds flagged tool-call rows embedded. After 0029 drops their vectors, deleting
    /// one must not fall into the unmapped-vector fallback (a full vec0 scan per row, which
    /// made deleting a long thread take minutes).
    #[test]
    fn deleting_a_legacy_tool_row_never_scans_the_vectors() {
        use rusqlite::StatementStatus;
        let mut conn = db_at_version(28);
        let (tid, _) = thread_with_messages(&conn, &[]);
        let mut legacy = Vec::new();
        for seq in 0..2 {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text, tool_name, embedded)
                 VALUES (?1, ?2, 'assistant', 'Bash: {}', 'Bash', 1)",
                params![tid, seq],
            )
            .unwrap();
            legacy.push(conn.last_insert_rowid());
        }
        MIGRATIONS.to_latest(&mut conn).unwrap();

        let delete_steps = |id: i64| -> i32 {
            let mut st = conn.prepare("DELETE FROM messages WHERE id = ?1").unwrap();
            st.execute([id]).unwrap();
            st.get_status(StatementStatus::VmStep)
        };
        let before = delete_steps(legacy[0]);
        let (_t2, more) = thread_with_messages(&conn, &[true; 2000]);
        for (i, id) in more.iter().enumerate() {
            store_chunk(&conn, *id, i);
        }
        assert_eq!(
            before,
            delete_steps(legacy[1]),
            "a legacy tool row delete must not scan vec_chunks"
        );
    }

    #[test]
    fn flag_updates_skip_fts_but_text_updates_reindex() {
        let conn = fresh_db();
        let trigger: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'messages_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(trigger.contains("UPDATE OF text"), "{trigger}");

        let (_tid, ids) = thread_with_messages(&conn, &[false]);
        let hits = |q: &str| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [q],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        conn.execute("UPDATE messages SET embedded = 1 WHERE id = ?1", [ids[0]])
            .unwrap();
        assert_eq!(hits("hello"), 1, "a flag update leaves the FTS row alone");
        conn.execute(
            "UPDATE messages SET text = 'goodbye' WHERE id = ?1",
            [ids[0]],
        )
        .unwrap();
        assert_eq!((hits("hello"), hits("goodbye")), (0, 1));
    }

    // ---- 0028: Windows Claude ids ------------------------------------------------------

    #[test]
    fn backslash_claude_ids_are_rewritten_once() {
        let mut conn = db_at_version(27);
        let source = |kind: &str| -> i64 {
            conn.query_row("SELECT id FROM sources WHERE kind = ?1", [kind], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let (claude, codex) = (source("claude_code"), source("codex"));
        for (sid, ext) in [
            (claude, r"C--proj\sess-1\subagents\agent-x.jsonl"),
            (claude, r"C--proj\dup.jsonl"),
            (claude, "C--proj/dup.jsonl"),
            (codex, r"not\a\claude\id"),
        ] {
            conn.execute(
                "INSERT INTO threads (source_id, external_id, title) VALUES (?1, ?2, 't')",
                params![sid, ext],
            )
            .unwrap();
        }
        conn.execute_batch(
            r"INSERT INTO index_state (path, source_kind, mtime, size)
              VALUES ('C:\Users\me\.claude\projects\C--proj\a.jsonl', 'claude_code', 1, 1),
                     ('/Users/me/.claude/projects/-proj/b.jsonl', 'claude_code', 1, 1);",
        )
        .unwrap();

        MIGRATIONS.to_latest(&mut conn).unwrap();

        let sub: (String, i64) = conn
            .query_row(
                "SELECT external_id, is_subagent FROM threads WHERE external_id LIKE '%agent-x%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(sub.0, "C--proj/sess-1/subagents/agent-x.jsonl");
        assert_eq!(sub.1, 1, "subagent flag recomputed from the fixed id");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM threads WHERE external_id = 'C--proj/dup.jsonl'"
            ),
            1,
            "the stale backslash duplicate is dropped, not renamed into a conflict"
        );
        assert_eq!(
            count(
                &conn,
                r"SELECT COUNT(*) FROM threads WHERE external_id = 'not\a\claude\id'"
            ),
            1,
            "other sources are untouched"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM index_state"),
            1,
            "only Windows-style cursors are forgotten"
        );
    }
}
