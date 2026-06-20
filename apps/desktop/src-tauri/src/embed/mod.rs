//! Local embeddings for semantic search. Uses fastembed (bge-small-en-v1.5, 384-dim)
//! entirely on-device. Messages are split into overlapping chunks (so long messages
//! are fully searchable, not truncated), each chunk embedded and stored in the
//! `vec_chunks` sqlite-vec (vec0) virtual table. KNN runs IN SQL — we never load the
//! whole vector set into Rust.

use anyhow::Result;
use fastembed::{EmbeddingModel as FeModel, InitOptions, TextEmbedding};
use rusqlite::{params, Connection, ToSql};
use std::sync::Mutex;

/// bge-small-en-v1.5 output dimensionality (matches vec_chunks `float[384]`).
pub const DIM: usize = 384;

/// bge retrieval works best with an instruction prefix on the QUERY only.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Only user/assistant messages are embedded — tool output is high-volume, low value.
const EMBED_ROLES: &str = "('user','assistant')";
/// Turn-aware chunking: ~400 tokens per chunk with overlap (chars ≈ 4×tokens).
/// Kept under bge-small's 512-token window with headroom for dense/code text;
/// larger chunks = fewer vectors to store and KNN-scan, with more context each.
const CHUNK_CHARS: usize = 1600;
const CHUNK_OVERLAP: usize = 200;

/// Lazily-initialized embedding model, held in Tauri managed state.
pub struct Embedder(Mutex<Option<TextEmbedding>>);

impl Default for Embedder {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl Embedder {
    /// Embed a batch of texts, loading the model on first call.
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut guard = self
            .0
            .lock()
            .map_err(|e| anyhow::anyhow!("embedder lock: {e}"))?;
        if guard.is_none() {
            // Cap ONNX intra-op parallelism so a build doesn't pin every core and
            // starve the UI thread. fastembed counts logical cores; leave 2 free.
            let threads = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(1);
            let model = TextEmbedding::try_new(
                InitOptions::new(FeModel::BGESmallENV15)
                    .with_show_download_progress(false)
                    .with_intra_threads(threads),
            )?;
            *guard = Some(model);
        }
        let model = guard.as_mut().unwrap();
        let out = model.embed(texts, None)?;
        debug_assert!(
            out.iter().all(|v| v.len() == DIM),
            "unexpected embedding dim"
        );
        Ok(out)
    }
}

/// Cap chunks per message. A giant pasted log (a 600 KB message is ~430 chunks at
/// CHUNK_CHARS) would otherwise stall the embed job and bloat the vector index; the
/// first N chunks capture the semantic gist, and FTS still searches the full text.
const MAX_CHUNKS_PER_MESSAGE: usize = 16;

/// Split text into overlapping char-windows. Short text stays a single chunk. Capped at
/// MAX_CHUNKS_PER_MESSAGE so one huge message can't dominate a batch.
fn chunk_text(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= CHUNK_CHARS {
        return vec![s.to_string()];
    }
    let step = CHUNK_CHARS - CHUNK_OVERLAP;
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + CHUNK_CHARS).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() || out.len() >= MAX_CHUNKS_PER_MESSAGE {
            break;
        }
        start += step;
    }
    out
}

// The embedding job is split into three phases so the DB lock is NEVER held during
// the (multi-second) model inference — only for the fast SELECT and the fast write.
// Holding the single `Mutex<Connection>` across inference would block every UI query
// (search, recent, stats…) and freeze the app. The background job in lib.rs drives
// these three; `embed_batch` below is the all-in-one convenience for tests.

/// Select up to `batch` not-yet-embedded user/assistant messages. The `embedded`
/// flag is maintained by a partial index, so this is O(batch) — no growing scan.
pub fn pending_batch(conn: &Connection, batch: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.text FROM messages m
         WHERE m.embedded = 0 AND m.role IN {EMBED_ROLES}
         LIMIT ?1"
    ))?;
    let rows = stmt
        .query_map([batch as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Flatten a batch of messages into chunks, tracking each chunk's owning message id.
/// Pure (no DB / no model) — run between the locked SELECT and the locked write.
pub fn chunk_messages(rows: &[(i64, String)]) -> (Vec<i64>, Vec<String>) {
    let mut owners = Vec::new();
    let mut texts = Vec::new();
    for (id, text) in rows {
        for c in chunk_text(text) {
            owners.push(*id);
            texts.push(c);
        }
    }
    (owners, texts)
}

/// Persist chunk vectors and mark the messages embedded, in one transaction. Fast —
/// the DB lock is held only for this, never for the inference that precedes it.
pub fn store_batch(
    conn: &mut Connection,
    message_ids: &[i64],
    owners: &[i64],
    vecs: &[Vec<f32>],
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut ins =
            tx.prepare("INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)")?;
        for (mid, v) in owners.iter().zip(vecs.iter()) {
            ins.execute(params![mid, vec_to_bytes(v)])?;
        }
        // Mark every selected message done (even any that produced no chunk) so it is
        // never reselected. Re-upserting a thread resets this via fresh rows.
        let mut done = tx.prepare("UPDATE messages SET embedded = 1 WHERE id = ?1")?;
        for id in message_ids {
            done.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Mark messages embedded WITHOUT storing vectors — for a batch that failed to embed, so
/// the job advances past it instead of retrying it forever. They stay FTS-searchable.
pub fn mark_embedded(conn: &Connection, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
    conn.execute(
        &format!("UPDATE messages SET embedded = 1 WHERE id IN ({placeholders})"),
        params.as_slice(),
    )?;
    Ok(())
}

/// Embed ONE batch end-to-end (select → chunk → embed → store) holding `conn`
/// throughout. Convenience for callers with no UI contention (tests, one-shots).
/// The app's background job uses the split phases above so it never holds the DB
/// lock during inference.
pub fn embed_batch(conn: &mut Connection, embedder: &Embedder, batch: usize) -> Result<usize> {
    let rows = pending_batch(conn, batch)?;
    if rows.is_empty() {
        return Ok(0);
    }
    let (owners, texts) = chunk_messages(&rows);
    let vecs = embedder.embed(texts)?;
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    store_batch(conn, &ids, &owners, &vecs)?;
    Ok(rows.len())
}

/// Embed pending decision/gotcha facts into `vec_facts` for cross-thread recall.
/// Batched with the DB lock released during inference (same discipline as the message
/// embed job). Called after distillation; a thread has only a handful of facts, so this
/// is cheap. Idempotent — only `embedded = 0` facts are processed.
pub fn embed_pending_facts(db: &crate::db::Db, embedder: &Embedder) -> Result<()> {
    const BATCH: usize = 64;
    loop {
        // 1. Locked, fast: claim a batch of un-embedded recall facts.
        let rows: Vec<(i64, String)> = {
            let conn = db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
            let mut stmt = conn.prepare(
                "SELECT id, text FROM facts
                 WHERE embedded = 0 AND kind IN ('decision', 'gotcha') LIMIT ?1",
            )?;
            let r = stmt
                .query_map([BATCH as i64], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            r
        };
        if rows.is_empty() {
            break;
        }
        // 2. UNLOCKED: model inference.
        let vecs = embedder.embed(rows.iter().map(|(_, t)| t.clone()).collect())?;
        // 3. Locked, fast: persist vectors + mark embedded.
        {
            let mut conn = db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
            let tx = conn.transaction()?;
            {
                let mut ins =
                    tx.prepare("INSERT INTO vec_facts (fact_id, embedding) VALUES (?1, ?2)")?;
                let mut done = tx.prepare("UPDATE facts SET embedded = 1 WHERE id = ?1")?;
                for ((id, _), v) in rows.iter().zip(vecs.iter()) {
                    ins.execute(params![id, vec_to_bytes(v)])?;
                    done.execute([*id])?;
                }
            }
            tx.commit()?;
        }
    }
    Ok(())
}

/// Embed a search query (with bge's query instruction prefix) into a single vector.
/// Runs the model — call this WITHOUT holding the DB lock so inference never blocks
/// other UI queries. Returns None for an empty/unembeddable query.
pub fn embed_query(embedder: &Embedder, query: &str) -> Result<Option<Vec<f32>>> {
    let qv = embedder.embed(vec![format!("{QUERY_PREFIX}{query}")])?;
    Ok(qv.into_iter().next())
}

/// Semantic KNN over chunk embeddings, deduped to message level, with source/
/// subagent filters applied. Embeds the query first, then runs the SQL KNN. Holds
/// `conn` across inference — prefer `embed_query` + `semantic_search_vec` on the
/// hot UI path so the DB lock isn't held during inference.
pub fn semantic_search(
    conn: &Connection,
    embedder: &Embedder,
    query: &str,
    include_subagents: bool,
    sources: &[String],
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    let Some(qv) = embed_query(embedder, query)? else {
        return Ok(Vec::new());
    };
    semantic_search_vec(conn, &qv, include_subagents, sources, k)
}

/// SQL-only semantic KNN given an already-embedded query vector. No model inference
/// here, so the DB lock is held only for the (fast) query. See `embed_query`.
pub fn semantic_search_vec(
    conn: &Connection,
    qv: &[f32],
    include_subagents: bool,
    sources: &[String],
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    // Over-fetch chunks so dedup + filtering still leaves k messages. vec0 applies
    // the source/subagent filter AFTER the KNN, so a selective source filter can
    // starve results — over-fetch far more candidates when one is active. sqlite-vec
    // requires `k` as a literal (not a bound parameter), so inline our own integer.
    let knn_k = if sources.is_empty() {
        (k * 5).max(200)
    } else {
        (k * 20).max(800)
    };

    // MATERIALIZED so SQLite doesn't inline the KNN into the outer query (vec0 only
    // permits `ORDER BY distance` on the KNN query itself, not the outer ORDER BY).
    let mut sql = format!(
        "WITH knn AS MATERIALIZED (
            SELECT message_id, distance FROM vec_chunks
            WHERE embedding MATCH ?1 AND k = {knn_k} ORDER BY distance
         )
         SELECT knn.message_id, MIN(knn.distance) AS d
         FROM knn
         JOIN messages m ON m.id = knn.message_id
         JOIN threads t ON t.id = m.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE 1=1"
    );
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(vec_to_bytes(qv))];
    if !include_subagents {
        sql.push_str(" AND t.is_subagent = 0");
    }
    if !sources.is_empty() {
        let placeholders: Vec<String> = sources
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect();
        sql.push_str(&format!(" AND s.kind IN ({})", placeholders.join(", ")));
        for s in sources {
            args.push(Box::new(s.clone()));
        }
    }
    sql.push_str(&format!(
        " GROUP BY knn.message_id ORDER BY d LIMIT ?{}",
        args.len() + 1
    ));
    args.push(Box::new(k as i64));

    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        let id: i64 = r.get(0)?;
        let dist: f64 = r.get(1)?;
        Ok((id, (1.0 - dist) as f32)) // cosine distance -> similarity
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Count of embedded messages vs total embeddable (for UI progress).
pub fn embed_progress(conn: &Connection) -> Result<(i64, i64)> {
    let done: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages WHERE embedded = 1 AND role IN {EMBED_ROLES}"),
        [],
        |r| r.get(0),
    )?;
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages WHERE role IN {EMBED_ROLES}"),
        [],
        |r| r.get(0),
    )?;
    Ok((done, total))
}

/// Encode an f32 vector as the little-endian byte BLOB sqlite-vec expects.
pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_short_text_once() {
        assert_eq!(chunk_text("hello").len(), 1);
    }

    #[test]
    fn chunks_long_text_with_overlap() {
        let s = "x".repeat(CHUNK_CHARS * 2 + 100);
        let chunks = chunk_text(&s);
        assert!(chunks.len() >= 3, "got {}", chunks.len());
        assert!(chunks.iter().all(|c| c.chars().count() <= CHUNK_CHARS));
    }

    #[test]
    fn chunks_capped_for_giant_message() {
        // A 600KB pasted-log message must not explode into hundreds of chunks (which
        // stalled the embed job). Capped at MAX_CHUNKS_PER_MESSAGE.
        let s = "abcd efgh ".repeat(60_000); // ~600KB
        let chunks = chunk_text(&s);
        assert_eq!(chunks.len(), MAX_CHUNKS_PER_MESSAGE, "giant message capped");
    }

    /// Real model + vec0 path. Downloads bge-small on first run (needs network):
    /// `cargo test -- --ignored embed_smoke --nocapture`
    #[test]
    #[ignore]
    fn embed_smoke() {
        let e = Embedder::default();
        let vecs = e
            .embed(vec!["how do I activate a python virtualenv".to_string()])
            .unwrap();
        assert_eq!(vecs[0].len(), DIM);
    }

    /// End-to-end: chunk + embed + vec0 KNN + hybrid on a tiny corpus.
    /// `cargo test -- --ignored hybrid_smoke --nocapture`
    #[test]
    #[ignore]
    fn hybrid_smoke() {
        let mut p = std::env::temp_dir();
        p.push(format!("callimachus_hybrid_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut conn = crate::db::open(&p).unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, 's1', 'venv')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        for (i, (role, text)) in [
            ("user", "how do I activate a python virtualenv"),
            (
                "assistant",
                "run source .venv/bin/activate to enter the environment",
            ),
            ("user", "the cat sat on the mat in the sun"),
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO messages (thread_id, seq, role, text) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![tid, i as i64, role, text],
            )
            .unwrap();
        }

        let embedder = Embedder::default();
        while embed_batch(&mut conn, &embedder, 8).unwrap() > 0 {}

        let sem = semantic_search(
            &conn,
            &embedder,
            "enter the shell environment",
            false,
            &[],
            3,
        )
        .unwrap();
        assert!(!sem.is_empty());
        let top_text: String = conn
            .query_row("SELECT text FROM messages WHERE id = ?1", [sem[0].0], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(top_text.contains("venv") || top_text.contains("environment"));

        let filters = crate::search::SearchFilters {
            hybrid: true,
            ..Default::default()
        };
        let hits =
            crate::search::hybrid(&conn, &embedder, "activate environment", &filters).unwrap();
        assert!(!hits.is_empty());
    }
}
