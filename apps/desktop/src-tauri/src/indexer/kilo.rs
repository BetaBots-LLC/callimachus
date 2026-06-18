//! Kilo Code indexer. Kilo is a Roo/Cline-lineage agent; its tasks live under the
//! `kilocode.kilo-code` globalStorage in the same `tasks/<id>/` layout, so we
//! delegate to the shared Cline-architecture engine in `cline`.

use super::{cline, IndexReport};
use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

pub const KIND: &str = "kilo";
pub const EXT_ID: &str = "kilocode.kilo-code";

pub fn scan(conn: &mut Connection) -> Result<IndexReport> {
    cline::scan_ext(conn, KIND, EXT_ID)
}

/// Kilo's task roots (used by the watcher).
pub fn task_roots() -> Vec<PathBuf> {
    cline::task_roots_for(EXT_ID)
}
