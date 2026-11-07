//! Small in-process caches for the analytics views (Stats heatmap, Stats, recurring issues,
//! spend). Those aggregate over the whole history and took seconds on a large index, yet
//! barely change minute to minute, and the views refetch every time they open.
//!
//! Entries expire after [`ANALYTICS_TTL`] and are keyed by the database file plus an
//! "index version" (thread count + newest thread id), so adding or deleting threads
//! invalidates them immediately while growth inside existing threads shows up within the
//! TTL. That trade-off is deliberate: keying on the newest message id instead would miss
//! on every watcher write during an active session, which is exactly when these views
//! are slow.

use rusqlite::Connection;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long an analytics result is reused.
pub(crate) const ANALYTICS_TTL: Duration = Duration::from_secs(10 * 60);

/// Entries kept per cache; the oldest is dropped beyond this (keys vary by project filter).
const MAX_ENTRIES: usize = 64;

/// Identifies "the same index in the same state" for cache purposes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct IndexVersion {
    db: String,
    threads: i64,
    max_thread_id: i64,
}

/// Cheap fingerprint of the thread set (one index-only COUNT plus a rowid MAX).
pub(crate) fn index_version(conn: &Connection) -> rusqlite::Result<IndexVersion> {
    let (threads, max_thread_id) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(id), 0) FROM threads",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(IndexVersion {
        db: conn.path().unwrap_or_default().to_string(),
        threads,
        max_thread_id,
    })
}

struct Entry<V> {
    at: Instant,
    value: V,
}

/// A TTL cache keyed by `(IndexVersion, K)`.
pub(crate) struct AggCache<K, V> {
    entries: Mutex<HashMap<(IndexVersion, K), Entry<V>>>,
}

impl<K: Eq + Hash + Clone, V: Clone> AggCache<K, V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The cached value for `key` at the index's current version, or `compute()`'s result
    /// (then cached). The lock is NOT held while computing, so a slow aggregate never
    /// blocks readers of other keys; two concurrent misses may both compute, which is
    /// harmless.
    pub(crate) fn get_or_try<E>(
        &self,
        version: IndexVersion,
        key: K,
        compute: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        let full_key = (version, key);
        if let Ok(map) = self.entries.lock() {
            if let Some(e) = map.get(&full_key) {
                if e.at.elapsed() < ANALYTICS_TTL {
                    return Ok(e.value.clone());
                }
            }
        }
        let value = compute()?;
        if let Ok(mut map) = self.entries.lock() {
            map.retain(|_, e| e.at.elapsed() < ANALYTICS_TTL);
            if map.len() >= MAX_ENTRIES {
                if let Some(oldest) = map.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) {
                    map.remove(&oldest);
                }
            }
            map.insert(
                full_key,
                Entry {
                    at: Instant::now(),
                    value: value.clone(),
                },
            );
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_until_the_thread_set_changes() {
        let p = std::env::temp_dir().join(format!("calli_aggcache_{}.db", std::process::id()));
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
        let conn = crate::db::open(&p).unwrap();
        let cache: AggCache<&str, i64> = AggCache::new();
        let mut calls = 0;
        let mut get = |conn: &Connection| {
            cache
                .get_or_try(index_version(conn).unwrap(), "k", || {
                    calls += 1;
                    Ok::<_, ()>(calls)
                })
                .unwrap()
        };
        assert_eq!(get(&conn), 1);
        assert_eq!(get(&conn), 1, "second read is served from the cache");
        conn.execute(
            "INSERT INTO threads (source_id, external_id) VALUES (1, 'new')",
            [],
        )
        .unwrap();
        assert_eq!(get(&conn), 2, "a new thread invalidates");
    }
}
