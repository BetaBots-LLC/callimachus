//! Schema migrations, embedded at compile time and run on startup.

use rusqlite_migration::{Migrations, M};
use std::sync::LazyLock;

pub static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        // 0001 — canonical store: sources, threads, messages, FTS5, index_state, providers.
        M::up(include_str!("../../migrations/0001_init.sql")),
        // 0002 — is_subagent flag on threads.
        M::up(include_str!("../../migrations/0002_subagent.sql")),
        // 0003 — embeddings table for semantic search.
        M::up(include_str!("../../migrations/0003_embeddings.sql")),
        // 0004 — sqlite-vec vec0 chunk index (replaces the embeddings BLOB table).
        M::up(include_str!("../../migrations/0004_vec0.sql")),
        // 0005 — Gemini CLI as an indexable source.
        M::up(include_str!("../../migrations/0005_gemini.sql")),
        // 0006 — Qwen Code, Goose, OpenCode, Continue, Cline sources.
        M::up(include_str!("../../migrations/0006_more_sources.sql")),
        // 0007 — Roo Code, Kilo Code (Cline-architecture forks).
        M::up(include_str!("../../migrations/0007_cline_forks.sql")),
        // 0008 — messages.embedded flag + partial index (fast incremental embedding).
        M::up(include_str!("../../migrations/0008_embed_flag.sql")),
        // 0009 — precomputed threads.bytes (fast cleanup list).
        M::up(include_str!("../../migrations/0009_thread_bytes.sql")),
        // 0010 — stars + free-form tags ("collections").
        M::up(include_str!("../../migrations/0010_stars_tags.sql")),
        // 0011 — distilled knowledge layer (facts: todos now, decisions/gotchas later).
        M::up(include_str!("../../migrations/0011_knowledge.sql")),
        // 0012 — opt-in LLM distillation tier: extraction state, fact vectors, config.
        M::up(include_str!("../../migrations/0012_knowledge_llm.sql")),
        // 0013 — file-path mentions for code-aware search.
        M::up(include_str!("../../migrations/0013_file_mentions.sql")),
        // 0014 — perf: composite list indexes, facts(thread_id,kind), trigram path FTS.
        M::up(include_str!("../../migrations/0014_perf.sql")),
        // 0015 — fact curation: pin / edit / hide distilled facts.
        M::up(include_str!("../../migrations/0015_fact_curation.sql")),
        // 0016 — canonical project key for stable per-project grouping.
        M::up(include_str!("../../migrations/0016_project_key.sql")),
        // 0017 — index messages.ts for the Coach activity heatmap range scan.
        M::up(include_str!("../../migrations/0017_messages_ts.sql")),
        // 0018 — track distillable (user/assistant) message count for distill staleness.
        M::up(include_str!("../../migrations/0018_distillable_count.sql")),
        // 0019 — agent session snapshots: resumable thread checkpoints for cross-agent handoff.
        M::up(include_str!("../../migrations/0019_snapshots.sql")),
        // 0020 — ADR-style decision rationale ("why") for the contradiction guard.
        M::up(include_str!("../../migrations/0020_decision_rationale.sql")),
    ])
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

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
}
