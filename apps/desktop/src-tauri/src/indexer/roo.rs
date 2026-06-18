//! Roo Code indexer. Roo is a Cline fork; its tasks live under the
//! `rooveterinaryinc.roo-cline` globalStorage in the same `tasks/<id>/` layout
//! with an `api_conversation_history.json` transcript, so we delegate to the
//! shared Cline-architecture engine in `cline`.

use super::{cline, IndexReport};
use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

pub const KIND: &str = "roo";
pub const EXT_ID: &str = "rooveterinaryinc.roo-cline";

pub fn scan(conn: &mut Connection) -> Result<IndexReport> {
    cline::scan_ext(conn, KIND, EXT_ID)
}

/// Roo's task roots (used by the watcher).
pub fn task_roots() -> Vec<PathBuf> {
    cline::task_roots_for(EXT_ID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{source_id, upsert_thread};

    const HISTORY: &str = r#"[
        {"role":"user","content":[{"type":"text","text":"roo recall sqlite fts5"}]},
        {"role":"assistant","content":[{"type":"text","text":"Sure, using FTS5"}]}
    ]"#;

    #[test]
    fn parses_via_shared_engine_under_roo_kind() {
        let hp = std::env::temp_dir().join(format!("calli_roo_{}.json", std::process::id()));
        std::fs::write(&hp, HISTORY).unwrap();
        let dbp = std::env::temp_dir().join(format!("calli_roo_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbp);
        let mut conn = crate::db::open(&dbp).unwrap();

        let sid = source_id(&conn, KIND).unwrap();
        let thread = cline::parse_history(&hp, "Code/1", None, None, None).unwrap().unwrap();
        upsert_thread(&mut conn, sid, &thread).unwrap();

        let hits =
            crate::search::search(&conn, "fts5", &crate::search::SearchFilters::default()).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.source == "roo"));
    }
}
