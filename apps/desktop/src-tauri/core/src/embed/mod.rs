//! Local embeddings for semantic search. Uses fastembed (bge-small-en-v1.5, 384-dim)
//! entirely on-device. Messages are split into overlapping chunks (so long messages
//! are fully searchable, not truncated), each chunk embedded and stored in the
//! `vec_chunks` sqlite-vec (vec0) virtual table. KNN runs IN SQL — we never load the
//! whole vector set into Rust.
//!
//! Model cache: the ~128 MB ONNX model lives in ONE absolute per-user directory
//! (`model_cache_dir`), shared by the app, `cal` and the MCP server. fastembed's own
//! default is `./.fastembed_cache` relative to the process CWD, which dropped a copy into
//! every repo a hook or MCP call ran in, and which the Finder-launched app (CWD `/`) could
//! not write at all. A cached model is loaded straight from its snapshot files, found in
//! any of the known cache roots (`lookup_roots`), so a sidecar whose environment differs
//! from the app's (`HF_HOME` set in the agent's shell) still finds the app's download.
//!
//! Loading vs. inference: a model that cannot be loaded (`EmbedError::ModelNotCached`,
//! `ModelLoading`, `ModelInit`) is reported separately from one input failing inference
//! (`EmbedError::Inference`). The background job must stop on the former and leave its
//! batch pending; only the latter may be skipped and marked embedded without vectors.

use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel as FeModel, InitOptionsUserDefined, TextEmbedding, TextInitOptions,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use rusqlite::{params, Connection, OptionalExtension, ToSql};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

/// bge-small-en-v1.5 output dimensionality (matches vec_chunks `float[384]`).
pub const DIM: usize = 384;

/// Cosine-similarity floor for the semantic arm: KNN always returns its k-nearest, so a
/// query with no good match would otherwise inject noise. Anything below this is dropped.
/// Shared so the hybrid fusion can weight by similarity over the same range.
///
/// bge-small scores everything high, so the old 0.35 never dropped a single hit: gibberish
/// came back with 180+ "matches". Calibrated on the 317k-vector index (Nov 2026) with 20
/// labeled queries, 10 real topics from recent thread titles and 10 nonsense strings:
/// - real: best hit 0.756-0.830; the labeled thread's hits at 0.728-0.830, in the top 5
///   for 8 of the 10 (the other two have no labeled hit in the top 15 at all, just closer
///   threads on the same topic);
/// - nonsense: best hit 0.571-0.648, except 'zzqxjvkw' at 0.728 and 0.707 (it lands on
///   `<task-notification>` and `z`-import rows), 0.60-0.68 down to the 15th.
///
/// At 0.70 the nonsense queries return 0 semantic hits (2 for 'zzqxjvkw') and every real
/// query keeps its top results. A single rare token ('notarytool', best 0.686) gets no
/// semantic hits, which is right: the keyword arm already finds it exactly.
pub(crate) const SEM_SIMILARITY_FLOOR: f32 = 0.70;

/// The floor for `search::related` (`cal related`, the VS Code panel, MCP): its input is a
/// code selection, an error or a long context, not a search-box query, and such text scores
/// lower against prose. SEM_SIMILARITY_FLOOR was calibrated only on short queries, and the
/// audit put related's at 0.62-0.65; 0.60 errs toward keeping a weak real match, and the
/// relative cutoff (applied there too) sheds the filler under a strong best hit.
pub(crate) const SEM_RELATED_FLOOR: f32 = 0.60;

/// Hybrid search drops semantic hits this far below the query's best one. On the same 20
/// queries each labeled thread's best hit was within 0.025 of the query's best, while a
/// query with a strong best hit sheds the 0.70-0.75 filler under it.
pub(crate) const SEM_RELATIVE_CUTOFF: f32 = 0.08;

/// The similarity at which a semantic hit keeps its full RRF weight in hybrid fusion: just
/// above the best hits real queries reach (0.83). Scaled to 1.0, even a query's best hit
/// kept only about three quarters of its weight.
pub(crate) const SEM_FULL_WEIGHT_SIM: f32 = 0.85;

/// bge retrieval works best with an instruction prefix on the QUERY only.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// SQL predicate for the messages that get embedded: user/assistant PROSE only.
///
/// Tool calls are stored as `role = 'assistant'` rows with `tool_name` set (for example
/// `Edit: {"file_path":...}`). They were ~85% of the pending backlog, cost hours of CPU,
/// and crowded real prose out of KNN results, while FTS already answers "what command did
/// I run". Tool output (`role = 'tool'`) is high-volume and low value.
///
/// Written against the `messages` table aliased as `m` (`FROM messages m`). Reuse this
/// exact text (don't retype it): SQLite only uses a partial index when the index's WHERE
/// terms appear in the query, so a hand-written variant can silently lose the index.
/// Mirrors `is_embeddable`, minus its blank-text and boilerplate checks: those are applied
/// by `chunk_messages` so the progress COUNTs never have to read message bodies.
pub const EMBEDDABLE_SQL: &str = "m.role IN ('user','assistant') AND m.tool_name IS NULL";

/// `messages.embedded` of a message the embedder is done with that got no vector: blank
/// text, harness boilerplate ([`is_boilerplate`]) or an input that failed inference. 0 is
/// pending and 1 has vectors, each with its owner row. Not 1: 0025's fallback trigger
/// (`messages_vec_ad_unmapped`) reads "embedded = 1, user/assistant prose, no owner row" as
/// vectors stored without the owner map and deletes them with a full vec0 scan under the
/// write lock (about 0.2 s per message at 200k vectors), so every deleted boilerplate
/// message (4,459 on a measured index) would pay one. Both of 0025's triggers skip this
/// value, and the progress counts take every non-zero value as done.
pub const EMBEDDED_NO_VECTOR: i64 = 2;

/// Rust-side twin of `EMBEDDABLE_SQL`: true for user/assistant prose that is worth a
/// vector. Blank text and harness boilerplate ([`is_boilerplate`]) are excluded too (they
/// would only produce meaningless vectors).
pub fn is_embeddable(role: &str, tool_name: Option<&str>, text: &str) -> bool {
    matches!(role, "user" | "assistant") && tool_name.is_none() && worth_a_vector(text)
}

/// Not blank and not [`is_boilerplate`]: the body half of [`is_embeddable`].
fn worth_a_vector(text: &str) -> bool {
    !text.trim().is_empty() && !is_boilerplate(text)
}

/// A message the agent harness wrote whole, which says nothing a search could want: the
/// local-command caveat, the interrupt markers, the session or weekly limit notice and a
/// slash command with no arguments. Among the 20 most repeated chunk texts of a 324k-vector index
/// (19% of its vectors repeated another chunk), with 1,054 + 357 + 2,028 + 1,020 messages.
/// FTS still finds them. Whole short messages only, so a typed message that merely starts
/// the same way still gets its vector. Subagent prompt templates (the other top repeats)
/// stay: they say what the subagent was asked to do, and [`ChunkMemo`] already runs each
/// one through the model once per build.
fn is_boilerplate(text: &str) -> bool {
    let t = text.trim();
    // The longest of each shape measured: 245, 42, 73 and 156 chars.
    (t.len() <= 400
        && t.starts_with("<local-command-caveat>")
        && t.ends_with("</local-command-caveat>"))
        || (t.len() <= 64 && t.starts_with("[Request interrupted by user") && t.ends_with(']'))
        || (t.len() <= 120 && t.starts_with("You've hit your ") && t.contains(" limit · resets"))
        || (t.len() <= 400
            && t.starts_with("<command-name>")
            && t.ends_with("<command-args></command-args>"))
}

/// Turn-aware chunking: ~400 tokens per chunk with overlap (chars ≈ 4×tokens).
/// Kept under bge-small's 512-token window with headroom for dense/code text;
/// larger chunks = fewer vectors to store and KNN-scan, with more context each.
const CHUNK_CHARS: usize = 1600;
const CHUNK_OVERLAP: usize = 200;

// ---------------------------------------------------------------------------------------
// Model cache location
// ---------------------------------------------------------------------------------------

/// The one model we ship. Its repo id and file names are read from fastembed's own model
/// table (see `cached_snapshot`), so the cache check can't drift from what fastembed
/// downloads.
const MODEL: FeModel = FeModel::BGESmallENV15;

/// Tokenizer files fastembed fetches next to the ONNX file (`load_tokenizer_hf_hub`).
const TOKENIZER_FILES: [&str; 4] = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

/// fastembed's own override variable. Honored first so existing setups (and CI) keep
/// working and a user can move the model wherever they like.
const CACHE_DIR_ENV: &str = "FASTEMBED_CACHE_DIR";

/// fastembed's `pull_from_hf` downloads into `HF_HOME`, used verbatim, in preference to
/// the directory we pass. So a download lands there when it is set, and the cache lookup
/// checks it (along with every other root).
const HF_HOME_ENV: &str = "HF_HOME";

/// Same bundle-id folder `db::default_index_path` puts index.db in.
const APP_DIR_NAME: &str = "dev.shaller.callimachus";

/// A non-empty path-valued environment variable.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where the model may live, best first. Every candidate is absolute: never CWD-relative.
fn cache_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // The per-user app-data base index.db uses, but deliberately NOT derived from
    // CALLIMACHUS_DB: pointing the index elsewhere (tests, a second profile) must not
    // split the shared model cache or trigger another 128 MB download.
    if let Some(base) = dirs::data_local_dir().or_else(dirs::home_dir) {
        out.push(base.join(APP_DIR_NAME).join("models"));
    }
    // Beside a relocated index, for setups where only that volume is writable (for
    // example a container with a read-only home and CALLIMACHUS_DB on a data volume).
    if let Some(parent) = crate::db::default_index_path()
        .parent()
        .filter(|p| p.is_absolute())
    {
        let beside_index = parent.join("models");
        if !out.contains(&beside_index) {
            out.push(beside_index);
        }
    }
    // Last resort: re-downloads after a temp cleanup, but never pollutes a repo.
    out.push(std::env::temp_dir().join("callimachus-models"));
    out
}

/// Pick the cache root. An explicit override is used as given; otherwise the first
/// candidate that is usable (exists, or could be created). Pure apart from `usable`, so
/// the policy is unit-testable without touching the real environment.
fn pick_cache_dir(
    env_override: Option<PathBuf>,
    candidates: &[PathBuf],
    usable: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(dir) = env_override {
        return dir;
    }
    candidates
        .iter()
        .find(|c| usable(c))
        .or_else(|| candidates.last())
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("callimachus-models"))
}

/// The configured cache root, resolved (and created) once per process.
fn configured_cache_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        pick_cache_dir(env_path(CACHE_DIR_ENV), &cache_dir_candidates(), |p| {
            std::fs::create_dir_all(p).is_ok()
        })
    })
}

/// Absolute directory the embedding model is downloaded into when the caller may
/// download: `$HF_HOME` when set (fastembed gives it precedence over everything),
/// `$FASTEMBED_CACHE_DIR`, else `<per-user app data>/dev.shaller.callimachus/models`, next
/// to index.db (`~/Library/Application Support/...` on macOS, `%LOCALAPPDATA%\...` on
/// Windows, `$XDG_DATA_HOME/...` on Linux). Loading looks further (`lookup_roots`).
pub fn model_cache_dir() -> PathBuf {
    env_path(HF_HOME_ENV).unwrap_or_else(|| configured_cache_dir().to_path_buf())
}

/// Every cache root a downloaded model may be in, best first: `$HF_HOME`,
/// `$FASTEMBED_CACHE_DIR`, then this build's own locations. All are checked, not just the
/// one this process would download into, because the app and its sidecars rarely share an
/// environment: the Dock-launched app sees no shell variables, while the hooks and MCP
/// server an agent spawns inherit them. Only absolute paths, so a lookup never resolves
/// against the agent's CWD.
fn lookup_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let candidates = [env_path(HF_HOME_ENV), env_path(CACHE_DIR_ENV)]
        .into_iter()
        .flatten()
        .chain(std::iter::once(configured_cache_dir().to_path_buf()))
        .chain(cache_dir_candidates());
    for root in candidates {
        if root.is_absolute() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// True when the model files are already on disk, so loading needs no network. Hooks,
/// `cal` and the MCP server check this so they never start a 128 MB download inside an
/// agent's prompt; when it is false they fall back to keyword (FTS) search.
pub fn model_is_cached() -> bool {
    find_snapshot(&lookup_roots()).is_some()
}

/// The first complete model snapshot under any of `roots`.
fn find_snapshot(roots: &[PathBuf]) -> Option<PathBuf> {
    roots.iter().find_map(|root| cached_snapshot(root))
}

/// The snapshot directory of a complete copy of `MODEL` under `cache_root`, in hf-hub's
/// cache layout: `models--<org>--<name>/refs/main` names a commit and
/// `snapshots/<commit>/<file>` exists for every file fastembed loads. hf-hub downloads
/// into `blobs/` and only links a file into `snapshots/` once it is complete, so presence
/// means a usable file.
fn cached_snapshot(cache_root: &Path) -> Option<PathBuf> {
    let info = TextEmbedding::get_model_info(&MODEL).ok()?;
    let repo = cache_root.join(format!("models--{}", info.model_code).replace('/', "--"));
    let commit = std::fs::read_to_string(repo.join("refs").join("main")).ok()?;
    let commit = commit.trim();
    if commit.is_empty() {
        return None;
    }
    let snapshot = repo.join("snapshots").join(commit);
    std::iter::once(info.model_file.as_str())
        .chain(info.additional_files.iter().map(String::as_str))
        .chain(TOKENIZER_FILES)
        .all(|f| snapshot.join(f).is_file())
        .then_some(snapshot)
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// Why an embedding call failed. Returned directly by the typed APIs (`ensure_loaded`,
/// `embed_rows`) and carried inside `anyhow::Error` by the rest (see `is_model_unavailable`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum EmbedError {
    /// The model is not on disk and this caller may not download it (a cached-only
    /// sidecar). Callers fall back to keyword search.
    #[error(
        "the embedding model is not downloaded yet (looked in {}); open Callimachus and build the semantic index to fetch it",
        join_paths(.0)
    )]
    ModelNotCached(Vec<PathBuf>),
    /// Another thread is loading (possibly downloading) the model right now.
    #[error("the embedding model is still loading")]
    ModelLoading,
    /// Loading or downloading the model failed (offline, proxy, full disk, unwritable
    /// cache dir, corrupt files). Retrying a batch can't help until the cause is fixed.
    #[error("could not load the embedding model: {0}")]
    ModelInit(String),
    /// The model loaded but inference on this input failed.
    #[error("embedding inference failed: {0}")]
    Inference(String),
}

impl EmbedError {
    /// True when the MODEL is unavailable (not downloaded, still loading, failed to load)
    /// rather than one input failing. A job seeing this must stop WITHOUT marking its batch
    /// embedded, so the batch is retried once the model works.
    pub fn is_model_unavailable(&self) -> bool {
        !matches!(self, Self::Inference(_))
    }
}

/// `EmbedError::is_model_unavailable` for an error that came back through `anyhow` (for
/// example from `Embedder::embed` or `embed_query`). False for any non-embedding error.
pub fn is_model_unavailable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<EmbedError>()
        .is_some_and(EmbedError::is_model_unavailable)
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn inference_error(e: anyhow::Error) -> EmbedError {
    EmbedError::Inference(format!("{e:#}"))
}

// ---------------------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------------------

/// After an implicit load fails, implicit loads fail fast for this long instead of
/// re-attempting (and possibly re-hitting the network) on every query or batch.
/// `ensure_loaded` ignores it, since it is an explicit "try now".
const INIT_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Max chunks per locked inference call on the bulk path. The model lock is held for one
/// sub-batch at a time (well under a second), so a search query waits at most that long
/// behind a running build instead of the whole multi-second batch.
const BULK_SUB_BATCH: usize = 16;

/// Upper bound on how long one bulk sub-batch defers to waiting queries, so a constant
/// stream of queries can slow the background build but never stall it.
const MAX_BULK_YIELD: Duration = Duration::from_secs(2);

/// ONNX intra-op threads for a query-only embedder (`cached_only`: hooks, `cal`, MCP).
/// One short query is fastest around here: on a 24-core M2 Ultra a query took 3-5 ms at 4
/// threads but 6-10 ms at the bulk setting (22), where thread sync outweighs the work. It
/// also spares each hook and MCP process ~18 idle ORT pool threads.
const QUERY_INTRA_THREADS: usize = 4;

/// ONNX intra-op threads for a background top-up build (the watcher's auto-embed after
/// agent activity), by its size ([`background_intra_threads`]). Measured on a 24-core M2
/// Ultra over real chunk texts (`background_threads_bench`), chunks per CPU-second barely
/// move with the thread count (100 chunks: 7.0 / 7.2 / 7.2 / 6.7 at 1 / 2 / 4 / 8 threads;
/// 2,000 chunks: 5.1 / 5.0 / 5.1 / 4.8), while 8 threads kept 7.3 cores busy for every
/// 16-100 chunk top-up the app logged (and spin between batches on top). A background
/// build is not waited on, so it takes the cheap end: 2 threads for a top-up of one claimed
/// batch (100 chunks in 7 s instead of 2 s), 4 for a longer backlog (2,000 chunks in 104 s
/// instead of 59 s). A Build click keeps `bulk_intra_threads`.
const BACKGROUND_INTRA_THREADS: usize = 4;

/// A top-up of up to this many messages (the job's claimed batch) runs on
/// [`BACKGROUND_SMALL_INTRA_THREADS`].
const BACKGROUND_SMALL_MESSAGES: i64 = 128;
const BACKGROUND_SMALL_INTRA_THREADS: usize = 2;

/// How long the app's shared (query) model stays loaded with nothing using it. Reloading
/// from the cache costs about 150 ms on the next query, while a loaded model holds about
/// 130 MB of weights plus ONNX Runtime working memory and its thread pool (the installed
/// app sat at a 1.6 GB footprint after 46 h because the model could never be dropped).
pub const APP_IDLE_UNLOAD: Duration = Duration::from_secs(10 * 60);

/// The same for `cached_only` sidecars (the MCP server). One semantic tool call took an
/// MCP process from about 30 MB to about 390 MB for the rest of the agent session, times
/// every concurrent session; a 150 ms reload is small next to an agent's think time.
pub const SIDECAR_IDLE_UNLOAD: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    /// `ensure_loaded`: always attempts (ignores the failure cooldown); downloads if the
    /// embedder allows it.
    Explicit,
    /// First use from `embed` / `embed_query`: waits for an in-progress load and honors
    /// the failure cooldown; downloads if the embedder allows it.
    Implicit,
    /// `embed_query_cached_only`: never downloads and never waits behind another thread's
    /// load, so a UI query can't block on a 128 MB download.
    CachedNoWait,
}

/// A loaded model. Every caller clones the `Arc` for the length of one call, so a strong
/// count of 1 (only the slot's own reference) means nobody is using it right now.
type Model = std::sync::Arc<Mutex<TextEmbedding>>;

/// The model in its slot, with when a caller last asked for it.
struct Loaded {
    model: Model,
    last_used: Instant,
}

/// Lazily-loaded embedding model, held in Tauri managed state (and owned directly by the
/// `cal` / MCP sidecars).
///
/// Concurrency: fastembed's `embed` needs `&mut`, so the loaded model sits behind a mutex,
/// but that lock is only ever held for ONE short sub-batch of inference, and queries take
/// priority over the bulk build between sub-batches (`interactive`). Loading has its own
/// lock so a slow first download never blocks a caller that already has a model.
///
/// Lifetime: the model can be dropped again (`unload_if_idle`) once nothing has used it
/// for `idle_unload`; the next call reloads it from the cache.
pub struct Embedder {
    /// The loaded model, if any.
    slot: Mutex<Option<Loaded>>,
    /// Serializes loading; holds the last load failure (when, why) for the cooldown.
    init: Mutex<Option<(Instant, String)>>,
    /// Interactive callers (queries) waiting for or using the model. The bulk path yields
    /// between sub-batches while this is non-zero. Shared with `bulk_build` embedders, so
    /// a build on its own model still steps aside for a query on this one.
    interactive: std::sync::Arc<AtomicUsize>,
    /// False for sidecars (hooks, MCP, CLI): load only from the cache, never download.
    allow_download: bool,
    /// Test-only cache root override: the only root looked in and downloaded into. `None`
    /// means `lookup_roots()` / `model_cache_dir()`.
    cache_root: Option<PathBuf>,
    /// ONNX intra-op threads the model is loaded with (`bulk_intra_threads` or
    /// `query_intra_threads`). Thread count does not change the vectors.
    intra_threads: usize,
    /// Drop the model after this long unused (`unload_if_idle`); `None` keeps it.
    idle_unload: Option<Duration>,
    /// Log loads and unloads to the app log (the GUI's embedders only: a sidecar's stderr
    /// is the agent's MCP log or terminal).
    log: bool,
    /// The last few query vectors, newest last ([`QUERY_VECTOR_CACHE`]).
    recent_queries: Mutex<std::collections::VecDeque<(String, Vec<f32>)>>,
}

/// Query vectors an embedder keeps. Each search box query is embedded by the hybrid search
/// and again by the facts lookup beside it (`search_facts`), at the same moment; a hit here
/// spares the second ONNX inference (tens of ms, behind the same model lock).
const QUERY_VECTOR_CACHE: usize = 8;

impl Default for Embedder {
    /// The app's shared embedder, for queries, recall and fact embeds: downloads the model
    /// on first use if it isn't cached yet, uses the small query thread pool (a one-text
    /// query is faster on 4 threads than on 22, see `QUERY_INTRA_THREADS`), and unloads
    /// after `APP_IDLE_UNLOAD`. The semantic-index build runs on its own `bulk_build`
    /// embedder.
    fn default() -> Self {
        let mut e = Self::new(true, None, query_intra_threads());
        e.idle_unload = Some(APP_IDLE_UNLOAD);
        e.log = true;
        e
    }
}

impl Embedder {
    fn new(allow_download: bool, cache_root: Option<PathBuf>, intra_threads: usize) -> Self {
        Self {
            slot: Mutex::new(None),
            init: Mutex::new(None),
            interactive: std::sync::Arc::new(AtomicUsize::new(0)),
            allow_download,
            cache_root,
            intra_threads,
            idle_unload: None,
            log: false,
            recent_queries: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// An embedder that NEVER downloads: for hooks, `cal` and the MCP server, which run
    /// inside an agent's prompt with the agent's CWD. Without a cached model,
    /// `embed_query` returns `Ok(None)` (callers fall back to keyword search),
    /// `embed_pending_facts*` leave facts pending for the app, and `embed` /
    /// `ensure_loaded` return `EmbedError::ModelNotCached`.
    ///
    /// Every caller embeds only queries and a handful of facts, so it loads with the small
    /// `query_intra_threads` pool instead of the app's bulk-build setting, and it may be
    /// unloaded `SIDECAR_IDLE_UNLOAD` after its last query (the MCP server's reaper).
    pub fn cached_only() -> Self {
        let mut e = Self::new(false, None, query_intra_threads());
        e.idle_unload = Some(SIDECAR_IDLE_UNLOAD);
        e
    }

    /// A separate embedder for one semantic-index build, dropped when the build ends so the
    /// bulk session's thread pool and the working memory its padded sub-batches grew go
    /// with it. `background`: the watcher's auto top-up, with about how many messages it
    /// will embed, which picks fewer threads ([`background_intra_threads`]). It downloads
    /// only if this one may, and shares this one's interactive counter so the build keeps
    /// yielding to queries. Never idle-unloaded: the build owns it.
    pub fn bulk_build(&self, background: Option<i64>) -> Self {
        let threads = match background {
            Some(pending) => background_intra_threads(pending),
            None => bulk_intra_threads(),
        };
        let mut e = Self::new(self.allow_download, self.cache_root.clone(), threads);
        e.interactive = std::sync::Arc::clone(&self.interactive);
        e.log = self.log;
        e
    }

    /// True once the model is loaded in this process.
    pub fn is_loaded(&self) -> bool {
        self.lock_slot().is_some()
    }

    /// Load the model now (downloading it if this embedder allows that and it isn't
    /// cached), retrying even if a recent attempt failed. Call it before a bulk job so a
    /// load failure aborts the job instead of being mistaken for N bad batches, and from a
    /// background thread to prefetch the model.
    pub fn ensure_loaded(&self) -> Result<(), EmbedError> {
        self.load(LoadMode::Explicit).map(|_| ())
    }

    /// Drop the model if it has been unused for this embedder's idle limit, no query is
    /// waiting for it and no call holds it. Returns whether it unloaded. Cheap when there is
    /// nothing to do, so a periodic tick can call it. A semantic build does not keep it:
    /// the build has its own model (`bulk_build`).
    pub fn unload_if_idle(&self) -> bool {
        let Some(limit) = self.idle_unload else {
            return false;
        };
        self.unload_idle_for(limit)
    }

    fn unload_idle_for(&self, limit: Duration) -> bool {
        if self.interactive.load(Ordering::Acquire) > 0 {
            return false;
        }
        let taken = {
            let mut slot = self.lock_slot();
            let idle = slot.as_ref().is_some_and(|l| {
                std::sync::Arc::strong_count(&l.model) == 1 && l.last_used.elapsed() >= limit
            });
            if !idle {
                return false;
            }
            slot.take()
        };
        // Dropped outside the slot lock: freeing the session and joining its thread pool
        // takes a few ms, and a query arriving meanwhile should go straight to its reload.
        if let Some(loaded) = taken {
            let idle_for = loaded.last_used.elapsed();
            drop(loaded);
            // Freed is not returned: malloc keeps the session's large buffers cached.
            crate::memtrim::release_freed_memory();
            if self.log {
                app_log!("[embed] model unloaded after {} s idle", idle_for.as_secs());
            }
        }
        true
    }

    /// Embed a batch of texts, loading the model on first call. Bulk-priority: runs in
    /// short sub-batches and yields to waiting queries between them. On failure the
    /// `anyhow::Error` carries an `EmbedError` (see `is_model_unavailable`).
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Ok(self.embed_typed(&texts)?)
    }

    /// `embed` with the typed error.
    fn embed_typed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.load(LoadMode::Implicit)?;
        self.infer_bulk(&model, texts)
    }

    /// Where a cached model is looked for.
    fn lookup_roots(&self) -> Vec<PathBuf> {
        match &self.cache_root {
            Some(root) => vec![root.clone()],
            None => lookup_roots(),
        }
    }

    /// Where a download goes.
    fn download_root(&self) -> PathBuf {
        self.cache_root.clone().unwrap_or_else(model_cache_dir)
    }

    fn lock_slot(&self) -> MutexGuard<'_, Option<Loaded>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The loaded model, marked used now, if there is one.
    fn loaded(&self) -> Option<Model> {
        let mut slot = self.lock_slot();
        let loaded = slot.as_mut()?;
        loaded.last_used = Instant::now();
        Some(std::sync::Arc::clone(&loaded.model))
    }

    /// Return the loaded model, loading it first if needed (subject to `mode` and
    /// `allow_download`).
    fn load(&self, mode: LoadMode) -> Result<Model, EmbedError> {
        if let Some(model) = self.loaded() {
            return Ok(model);
        }
        let mut last_failure = match mode {
            LoadMode::CachedNoWait => match self.init.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => return Err(EmbedError::ModelLoading),
            },
            LoadMode::Explicit | LoadMode::Implicit => {
                self.init.lock().unwrap_or_else(PoisonError::into_inner)
            }
        };
        // Another thread may have finished loading while we waited for the init lock.
        if let Some(model) = self.loaded() {
            return Ok(model);
        }
        if mode != LoadMode::Explicit {
            if let Some((at, why)) = last_failure.as_ref() {
                if at.elapsed() < INIT_RETRY_AFTER {
                    return Err(EmbedError::ModelInit(why.clone()));
                }
            }
        }
        let may_download = self.allow_download && mode != LoadMode::CachedNoWait;
        let roots = self.lookup_roots();
        let clock = crate::jobclock::JobClock::start();
        let loaded = match find_snapshot(&roots) {
            Some(snapshot) => load_snapshot(&snapshot, self.intra_threads),
            None if may_download => download_model(&self.download_root(), self.intra_threads),
            // Not a failure (nothing was attempted), so it doesn't start the cooldown.
            None => return Err(EmbedError::ModelNotCached(roots)),
        };
        match loaded {
            Ok(model) => {
                *last_failure = None;
                if self.log {
                    app_log!(
                        "[embed] model loaded in {} ({} threads)",
                        clock.process_summary(),
                        self.intra_threads
                    );
                }
                let model: Model = std::sync::Arc::new(Mutex::new(model));
                *self.lock_slot() = Some(Loaded {
                    model: std::sync::Arc::clone(&model),
                    last_used: Instant::now(),
                });
                Ok(model)
            }
            Err(e) => {
                let why = format!("{e:#}");
                *last_failure = Some((Instant::now(), why.clone()));
                Err(EmbedError::ModelInit(why))
            }
        }
    }

    /// Bulk inference: one short locked call per sub-batch, deferring to queries between.
    ///
    /// Sub-batches are cut in length order, not arrival order. fastembed pads each batch
    /// to its longest member, and the pending backlog is skewed (p50 ~75 tokens, p90 ~446),
    /// so arrival-order batches ran ~3x the real tokens. Grouping similar lengths about
    /// doubles chunks/s and shortens the worst model-lock hold a query can wait behind.
    /// Vectors are written back to their input positions, so the output stays parallel
    /// to `texts` (callers zip it with owner ids).
    fn infer_bulk(
        &self,
        model: &Mutex<TextEmbedding>,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = vec![Vec::new(); texts.len()];
        for idxs in length_order(texts).chunks(BULK_SUB_BATCH) {
            self.yield_to_interactive();
            let sub: Vec<&str> = idxs.iter().map(|&i| texts[i].as_str()).collect();
            let vecs = lock_model(model)
                .embed(&sub, None)
                .map_err(inference_error)?;
            for (&i, v) in idxs.iter().zip(vecs) {
                out[i] = v;
            }
        }
        debug_assert!(
            out.iter().all(|v| v.len() == DIM),
            "unexpected embedding dim"
        );
        Ok(out)
    }

    /// Interactive inference (queries): registers as waiting so the bulk path steps aside
    /// after its current sub-batch, then runs in a single call.
    /// One query's vector (`query` trimmed, without the prefix), from the recent-query cache
    /// when this embedder already embedded it. Checked again once the model lock is held:
    /// the two callers of one keystroke usually arrive together, and the second waits there
    /// while the first embeds.
    fn infer_query(
        &self,
        model: &Mutex<TextEmbedding>,
        query: &str,
    ) -> Result<Option<Vec<f32>>, EmbedError> {
        let cached = || {
            self.recent_queries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|(q, _)| q == query)
                .map(|(_, v)| v.clone())
        };
        if let Some(v) = cached() {
            return Ok(Some(v));
        }
        let _ticket = InteractiveTicket::enter(&self.interactive);
        let mut guard = lock_model(model);
        if let Some(v) = cached() {
            return Ok(Some(v));
        }
        let text = format!("{QUERY_PREFIX}{query}");
        let out = guard
            .embed(std::slice::from_ref(&text), None)
            .map_err(inference_error)?;
        drop(guard);
        let Some(v) = out.into_iter().next() else {
            return Ok(None);
        };
        let mut recent = self
            .recent_queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if recent.len() >= QUERY_VECTOR_CACHE {
            recent.pop_front();
        }
        recent.push_back((query.to_string(), v.clone()));
        Ok(Some(v))
    }

    /// Embed texts as an interactive caller, uncached (the bulk-vs-query equivalence test).
    #[cfg(test)]
    fn infer_interactive(
        &self,
        model: &Mutex<TextEmbedding>,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let _ticket = InteractiveTicket::enter(&self.interactive);
        let out = lock_model(model)
            .embed(texts, None)
            .map_err(inference_error)?;
        debug_assert!(
            out.iter().all(|v| v.len() == DIM),
            "unexpected embedding dim"
        );
        Ok(out)
    }

    /// Bulk side of the priority scheme: wait (bounded) while any query is waiting for or
    /// using the model. std's Mutex isn't fair, so without this a tight bulk loop could
    /// re-take the lock ahead of a waiting query over and over.
    fn yield_to_interactive(&self) {
        let deadline = Instant::now() + MAX_BULK_YIELD;
        while self.interactive.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Logical cores (fastembed counts these too), or 1 if unknown.
fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// ONNX intra-op threads for the app's bulk build: capped so a build doesn't pin every
/// core and starve the UI thread; leave 2 free. Throughput still scales to here (79
/// chunks/s at 22 threads vs 55 at 8 on a 24-core machine), so the app keeps n-2.
fn bulk_intra_threads() -> usize {
    available_cores().saturating_sub(2).max(1)
}

/// ONNX intra-op threads for a background top-up of about `pending` messages (see
/// [`BACKGROUND_INTRA_THREADS`]), never more than a Build click would use.
fn background_intra_threads(pending: i64) -> usize {
    let threads = if pending <= BACKGROUND_SMALL_MESSAGES {
        BACKGROUND_SMALL_INTRA_THREADS
    } else {
        BACKGROUND_INTRA_THREADS
    };
    bulk_intra_threads().min(threads)
}

/// ONNX intra-op threads for a query-only embedder (see `QUERY_INTRA_THREADS`).
fn query_intra_threads() -> usize {
    available_cores().min(QUERY_INTRA_THREADS)
}

/// Load bge-small from a complete snapshot directory (see `cached_snapshot`), reading the
/// files directly. Going through fastembed's hf-hub loader instead would re-resolve the
/// cache from `$HF_HOME`, which may point somewhere else entirely. Same session options,
/// tokenizer, pooling and quantization as `TextEmbedding::try_new`, so the vectors match.
fn load_snapshot(snapshot: &Path, intra_threads: usize) -> Result<TextEmbedding> {
    let context = || format!("loading bge-small-en-v1.5 from {}", snapshot.display());
    let info = TextEmbedding::get_model_info(&MODEL).with_context(context)?;
    let read = |file: &str| {
        let path = snapshot.join(file);
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    };
    let tokenizer = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let mut model = UserDefinedEmbeddingModel::new(read(&info.model_file)?, tokenizer)
        .with_quantization(TextEmbedding::get_quantization_mode(&MODEL));
    if let Some(pooling) = TextEmbedding::get_default_pooling_method(&MODEL) {
        model = model.with_pooling(pooling);
    }
    model.output_key = info.output_key.clone();
    // External weight files (none for bge-small) are referenced by bare file name.
    for file in &info.additional_files {
        let name = Path::new(file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.clone());
        model = model.with_external_initializer(name, read(file)?);
    }
    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new().with_intra_threads(intra_threads),
    )
    .with_context(context)
}

/// Download bge-small into `cache_root` (an absolute dir) through fastembed's hf-hub
/// loader, then load it. Only the app does this; callers decide beforehand whether a
/// download is allowed.
fn download_model(cache_root: &Path, intra_threads: usize) -> Result<TextEmbedding> {
    // hf-hub uses `$HF_HOME` verbatim when it is set, in preference to `cache_root`. An
    // empty or relative value would put 128 MB under this process's CWD, so refuse it.
    if let Some(hf_home) = std::env::var_os(HF_HOME_ENV) {
        if !Path::new(&hf_home).is_absolute() {
            anyhow::bail!(
                "{HF_HOME_ENV} is set to {hf_home:?}, which is not an absolute path; set it \
                 to an absolute directory or unset it to download the embedding model"
            );
        }
    }
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("creating model cache dir {}", cache_root.display()))?;
    TextEmbedding::try_new(
        TextInitOptions::new(MODEL)
            .with_cache_dir(cache_root.to_path_buf())
            .with_show_download_progress(false)
            .with_intra_threads(intra_threads),
    )
    .with_context(|| format!("loading bge-small-en-v1.5 from {}", cache_root.display()))
}

/// Indices of `texts`, shortest first. Byte length tracks token count closely enough to
/// group similar inputs, and the stable sort keeps equal lengths in arrival order.
fn length_order(texts: &[String]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| texts[i].len());
    order
}

/// Lock the loaded model. A panic during inference poisons the mutex; the ONNX session
/// keeps no half-updated state we depend on, so keep serving instead of failing every
/// later call for the rest of the process.
fn lock_model(model: &Mutex<TextEmbedding>) -> MutexGuard<'_, TextEmbedding> {
    model.lock().unwrap_or_else(PoisonError::into_inner)
}

/// RAII registration of an interactive caller (see `Embedder::yield_to_interactive`).
struct InteractiveTicket<'a>(&'a AtomicUsize);

impl<'a> InteractiveTicket<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for InteractiveTicket<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

// ---------------------------------------------------------------------------------------
// Chunking + the message embedding job
// ---------------------------------------------------------------------------------------

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

/// Select up to `batch` not-yet-embedded messages that pass `EMBEDDABLE_SQL`. 0026's
/// covering (role, tool_name, embedded) index serves this predicate (0031 dropped 0008's
/// partial index), so this is O(batch), not a growing scan.
pub fn pending_batch(conn: &Connection, batch: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.text FROM messages m
         WHERE m.embedded = 0 AND {EMBEDDABLE_SQL}
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
/// Blank and boilerplate messages ([`is_boilerplate`]) produce no chunk; `store_batch`
/// still marks them done.
pub fn chunk_messages(rows: &[(i64, String)]) -> (Vec<i64>, Vec<String>) {
    let mut owners = Vec::new();
    let mut texts = Vec::new();
    for (id, text) in rows {
        if !worth_a_vector(text) {
            continue;
        }
        for c in chunk_text(text) {
            owners.push(*id);
            texts.push(c);
        }
    }
    (owners, texts)
}

/// Vectors for one claimed batch of messages (see `embed_rows`).
#[derive(Debug, Default)]
pub struct EmbeddedBatch {
    /// Owning message id of each vector, parallel to `vecs`.
    pub owners: Vec<i64>,
    /// One vector per chunk.
    pub vecs: Vec<Vec<f32>>,
    /// The text key of each vector, parallel to `vecs`, for [`ChunkMemo::remember`].
    pub keys: Vec<ChunkKey>,
    /// Messages whose inference failed even when retried on their own. They have no
    /// vectors; `store_batch` still marks them embedded so the job moves past them (FTS
    /// still finds them).
    pub failed: Vec<i64>,
    /// Vectors that did not go through the model: a repeat of a text earlier in the batch,
    /// or one the build had already stored ([`ChunkMemo`]).
    pub reused: usize,
}

/// A chunk text's identity for [`ChunkMemo`]: a 64-bit hash of the text with its length.
/// Two different texts would have to collide on both to share a vector; with the memo's
/// 200k entries the chance is about 1e-9 per build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    hash: u64,
    len: usize,
}

impl ChunkKey {
    pub fn of(text: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        Self {
            hash: h.finish(),
            len: text.len(),
        }
    }
}

/// Most chunk texts one build remembers: about 10 MB of map at 200k entries.
const CHUNK_MEMO_CAP: usize = 200_000;

/// The chunk texts one build already stored, by key, with the rowid of their vector, so a
/// repeat copies that vector instead of running the model. 19% of the 324k vectors on a
/// measured index were exact repeats of another chunk, mostly the prompts a workflow sends
/// to many subagents (one text 1,277 times) and harness notices ([`is_boilerplate`], now
/// skipped outright). The model gives a text the
/// same vector every time (thread count and batch padding move it by under 1e-5 in cosine),
/// so a copy is what inference would have stored. Per build, not persisted: no migration,
/// and a stale entry (its message deleted since) only costs a re-embed. Keeps its first
/// [`CHUNK_MEMO_CAP`] entries; the repeats worth catching show up early and often.
#[derive(Default)]
pub struct ChunkMemo {
    stored: std::collections::HashMap<ChunkKey, i64>,
}

impl ChunkMemo {
    /// The chunks of `rows` this build has stored before, as (key, rowid). Pure: run it
    /// without the DB lock, then [`read_vectors`](Self::read_vectors) under it.
    pub fn reusable(&self, rows: &[(i64, String)]) -> Vec<(ChunkKey, i64)> {
        if self.stored.is_empty() {
            return Vec::new();
        }
        let (_, texts) = chunk_messages(rows);
        let mut out: Vec<(ChunkKey, i64)> = Vec::new();
        for t in &texts {
            let key = ChunkKey::of(t);
            if let Some(&rowid) = self.stored.get(&key) {
                if !out.iter().any(|(k, _)| *k == key) {
                    out.push((key, rowid));
                }
            }
        }
        out
    }

    /// Read the stored vectors of `hits` back (vec0 point lookups by rowid, a few
    /// microseconds each). A vector that is gone (its message was deleted or re-indexed
    /// since) is forgotten, and its text is embedded again.
    pub fn read_vectors(
        &mut self,
        conn: &Connection,
        hits: &[(ChunkKey, i64)],
    ) -> std::collections::HashMap<ChunkKey, Vec<f32>> {
        let mut out = std::collections::HashMap::new();
        let Ok(mut stmt) = conn.prepare_cached("SELECT embedding FROM vec_chunks WHERE rowid = ?1")
        else {
            return out;
        };
        for (key, rowid) in hits {
            match stmt
                .query_row([rowid], |r| r.get::<_, Vec<u8>>(0))
                .optional()
            {
                Ok(Some(blob)) if blob.len() == DIM * 4 => {
                    out.insert(*key, bytes_to_vec(&blob));
                }
                _ => {
                    self.stored.remove(key);
                }
            }
        }
        out
    }

    /// Remember the vectors `store_batch` just wrote: `keys` and `rowids` are parallel to
    /// the batch's vectors (a None rowid was not stored).
    pub fn remember(&mut self, keys: &[ChunkKey], rowids: &[Option<i64>]) {
        for (key, rowid) in keys.iter().zip(rowids) {
            if self.stored.len() >= CHUNK_MEMO_CAP {
                return;
            }
            if let Some(rowid) = rowid {
                self.stored.entry(*key).or_insert(*rowid);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.stored.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }
}

/// Vectors for `texts` (keyed by `keys`, parallel), running the model once per distinct
/// text that `known` does not already hold. Returns them parallel to `texts`, with how many
/// skipped the model.
fn embed_unique(
    embedder: &Embedder,
    texts: &[String],
    keys: &[ChunkKey],
    known: &std::collections::HashMap<ChunkKey, Vec<f32>>,
) -> Result<(Vec<Vec<f32>>, usize), EmbedError> {
    embed_unique_with(texts, keys, known, |todo| embedder.embed_typed(todo))
}

/// [`embed_unique`] with the inference step passed in (tests count what reaches it).
fn embed_unique_with(
    texts: &[String],
    keys: &[ChunkKey],
    known: &std::collections::HashMap<ChunkKey, Vec<f32>>,
    infer: impl FnOnce(&[String]) -> Result<Vec<Vec<f32>>, EmbedError>,
) -> Result<(Vec<Vec<f32>>, usize), EmbedError> {
    let mut slot: std::collections::HashMap<ChunkKey, usize> = std::collections::HashMap::new();
    let mut todo: Vec<String> = Vec::new();
    for (text, key) in texts.iter().zip(keys) {
        if known.contains_key(key) || slot.contains_key(key) {
            continue;
        }
        slot.insert(*key, todo.len());
        todo.push(text.clone());
    }
    let fresh = infer(&todo)?;
    let vecs = keys
        .iter()
        .map(|key| match known.get(key) {
            Some(v) => v.clone(),
            None => fresh[slot[key]].clone(),
        })
        .collect();
    Ok((vecs, texts.len() - todo.len()))
}

/// Chunk and embed a claimed batch (the unlocked middle phase of the job), isolating bad
/// inputs: if the batch fails inference, each message is retried alone and only the ones
/// that still fail land in `failed`. A text repeated within the batch runs through the
/// model once.
///
/// Returns `Err` only when the MODEL is unavailable (`EmbedError::is_model_unavailable`).
/// The caller must then stop the job and leave the rows pending (do NOT mark them
/// embedded), so they are embedded once the model loads.
pub fn embed_rows(
    embedder: &Embedder,
    rows: &[(i64, String)],
) -> Result<EmbeddedBatch, EmbedError> {
    embed_rows_with(embedder, rows, &std::collections::HashMap::new())
}

/// [`embed_rows`] given the vectors a build already has for some chunk texts
/// ([`ChunkMemo::read_vectors`]): those are copied instead of embedded.
pub fn embed_rows_with(
    embedder: &Embedder,
    rows: &[(i64, String)],
    known: &std::collections::HashMap<ChunkKey, Vec<f32>>,
) -> Result<EmbeddedBatch, EmbedError> {
    let (owners, texts) = chunk_messages(rows);
    let keys: Vec<ChunkKey> = texts.iter().map(|t| ChunkKey::of(t)).collect();
    match embed_unique(embedder, &texts, &keys, known) {
        Ok((vecs, reused)) => {
            return Ok(EmbeddedBatch {
                owners,
                vecs,
                keys,
                failed: Vec::new(),
                reused,
            })
        }
        Err(e) if e.is_model_unavailable() => return Err(e),
        Err(_) => {} // one input broke the batch: isolate it below
    }
    let mut batch = EmbeddedBatch::default();
    for row in rows {
        let (owners, texts) = chunk_messages(std::slice::from_ref(row));
        let keys: Vec<ChunkKey> = texts.iter().map(|t| ChunkKey::of(t)).collect();
        match embed_unique(embedder, &texts, &keys, known) {
            Ok((vecs, reused)) => {
                batch.owners.extend(owners);
                batch.vecs.extend(vecs);
                batch.keys.extend(keys);
                batch.reused += reused;
            }
            Err(e) if e.is_model_unavailable() => return Err(e),
            Err(_) => batch.failed.push(row.0),
        }
    }
    Ok(batch)
}

/// `vec_chunks` as `cleanup::rebuild_vec_chunks` recreates it: 0004's table plus a
/// `subagent` partition key (0 for main threads, 1 for subagent ones, from
/// `threads.is_subagent`). vec0 keeps each partition in its own chunks and a KNN constrained
/// to `subagent = 0` reads only the main partition's. Before this, a default search ran a
/// global KNN and dropped subagent hits afterwards: on a 324k-vector index where 58% of the
/// vectors are subagent ones, 1 of 5 sampled queries (2 of 5 real ones in an earlier
/// survey) kept 0 main-thread hits out of the over-fetch; partitioned, all five got their
/// 60 (`real_index_default_knn`).
///
/// `gen` ([`VEC_GEN`]) is there to be mandatory. vec0 takes an insert that leaves the
/// partition key out: the vector lands in a NULL partition that no default KNN reads, in a
/// 1024-slot chunk of its own (about 1.5 MB per vector). 0036's version bump keeps an older
/// binary from opening the index, but one that already had it open when the table is rebuilt
/// (an app left running, a `cal watch`, a `cal embed` mid-run) never looks at the version
/// again and goes on inserting `(message_id, embedding)`. vec0 refuses a NULL in an INTEGER
/// metadata column, so that insert now fails ("Expected integer for INTEGER metadata column
/// gen"), its batch rolls back, and nothing is marked embedded until that process is
/// restarted. Older readers are not affected: their KNN names no new column.
pub(crate) const PARTITIONED_VEC_CHUNKS_SQL: &str = "CREATE VIRTUAL TABLE vec_chunks USING vec0(
    message_id integer,
    subagent integer partition key,
    gen integer,
    embedding float[384] distance_metric=cosine
)";

/// The `gen` every vector is stored with: the generation of the writer and model that made
/// it. Its job today is to exist (see [`PARTITIONED_VEC_CHUNKS_SQL`]); a later change of the
/// embedding model or of the stored format can bump it to tell old vectors from new ones.
pub const VEC_GEN: i64 = 1;

/// Whether `vec_chunks` has its current shape ([`PARTITIONED_VEC_CHUNKS_SQL`], the `subagent`
/// partition and `gen`), read from its stored CREATE statement. Migration 0036 leaves the
/// old shape in place and a background job rebuilds it later, so readers and writers meet
/// both: a pooled reader that outlives the rebuild, a sidecar on an index whose app has not
/// rebuilt yet. Any other statement reads as not partitioned, and the rebuild is then due. A
/// cached lookup on the schema table, per call, so it follows a rebuild that lands under a
/// long-lived connection.
pub fn vec_partitioned(conn: &Connection) -> bool {
    conn.prepare_cached(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'vec_chunks'",
    )
    .and_then(|mut stmt| stmt.query_row([], |r| r.get::<_, String>(0)))
    .is_ok_and(|sql| is_partitioned_ddl(&sql))
}

/// Whether a stored CREATE statement is [`PARTITIONED_VEC_CHUNKS_SQL`], whitespace and case
/// aside (SQLite keeps the statement's text from the table name on).
fn is_partitioned_ddl(sql: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    };
    norm(sql) == norm(PARTITIONED_VEC_CHUNKS_SQL)
}

/// Persist chunk vectors and mark the messages embedded, in one transaction. Fast —
/// the DB lock is held only for this, never for the inference that precedes it.
///
/// Every vector gets a `vec_chunk_owner` row (migration 0025), so deleting its message
/// removes it by rowid instead of a full vec0 scan. A message that was deleted or
/// re-upserted while the batch was being embedded (the inference runs unlocked) gets no
/// vectors, so none are orphaned. Never `DELETE FROM vec_chunks` directly: delete through
/// `vec_chunk_owner`, whose trigger removes the vector, or a stale owner row could later
/// point at a reused rowid.
///
/// On a partitioned table each vector goes into its thread's partition, with [`VEC_GEN`].
/// Never insert one without its partition: a NULL partition value is a partition no KNN
/// reads, and vec0 starts a new 1024-slot chunk for every such insert (its latest-chunk
/// lookup never matches NULL). The old statement cannot do that by accident (a failed shape
/// probe here, an older process): it omits `gen`, which vec0 refuses.
///
/// Returns the stored rowid of each vector, parallel to `owners` (None where the message
/// went away), so a build can reuse them for repeated chunk texts ([`ChunkMemo`]).
///
/// A selected message that got no vector (no chunk, or failed inference) is marked
/// [`EMBEDDED_NO_VECTOR`], not 1, so deleting it never runs 0025's full-scan fallback.
pub fn store_batch(
    conn: &mut Connection,
    message_ids: &[i64],
    owners: &[i64],
    vecs: &[Vec<f32>],
) -> Result<Vec<Option<i64>>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut rowids = Vec::with_capacity(owners.len());
    {
        let partitioned = vec_partitioned(&tx);
        // The thread's subagent flag, or nothing when the message is gone or already
        // embedded (re-upserted mid-batch).
        let mut live = tx.prepare(
            "SELECT t.is_subagent FROM messages m JOIN threads t ON t.id = m.thread_id
             WHERE m.id = ?1 AND m.embedded = 0",
        )?;
        let mut ins = tx.prepare(if partitioned {
            "INSERT INTO vec_chunks (message_id, subagent, gen, embedding)
             VALUES (?1, ?2, ?3, ?4)"
        } else {
            "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)"
        })?;
        let mut own = tx.prepare(
            "INSERT OR REPLACE INTO vec_chunk_owner (chunk_rowid, message_id) VALUES (?1, ?2)",
        )?;
        let mut alive: std::collections::HashMap<i64, Option<i64>> =
            std::collections::HashMap::new();
        // Messages this transaction stored at least one vector for.
        let mut vectored: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for (mid, v) in owners.iter().zip(vecs.iter()) {
            let subagent = match alive.get(mid) {
                Some(s) => *s,
                None => {
                    let s = live
                        .query_row([mid], |r| r.get::<_, i64>(0))
                        .optional()?
                        .map(|s| i64::from(s != 0));
                    alive.insert(*mid, s);
                    s
                }
            };
            let Some(subagent) = subagent else {
                rowids.push(None);
                continue; // deleted / re-upserted mid-batch: no orphan vectors
            };
            if partitioned {
                ins.execute(params![mid, subagent, VEC_GEN, vec_to_bytes(v)])?;
            } else {
                ins.execute(params![mid, vec_to_bytes(v)])?;
            }
            let rowid = tx.last_insert_rowid();
            own.execute(params![rowid, mid])?;
            rowids.push(Some(rowid));
            vectored.insert(*mid);
        }
        // Mark every selected message done (even any that produced no chunk) so it is
        // never reselected. Re-upserting a thread resets this via fresh rows. Only rows
        // still pending: one another build already finished keeps its state (a 1 turned
        // into EMBEDDED_NO_VECTOR would leave its vectors to the orphan sweep on delete).
        let mut done =
            tx.prepare("UPDATE messages SET embedded = ?2 WHERE id = ?1 AND embedded = 0")?;
        for id in message_ids {
            let state = if vectored.contains(id) {
                1
            } else {
                EMBEDDED_NO_VECTOR
            };
            done.execute(params![id, state])?;
        }
    }
    tx.commit()?;
    Ok(rowids)
}

/// Mark messages embedded WITHOUT storing vectors, so the job advances past inputs that
/// failed INFERENCE instead of retrying them forever. They stay FTS-searchable.
///
/// Never call this for a model-load failure (`is_model_unavailable`): that would flag the
/// whole corpus embedded with no vectors. Stop the job and leave the batch pending.
/// Marks them [`EMBEDDED_NO_VECTOR`], as `store_batch` does a message without a vector.
pub fn mark_embedded(conn: &Connection, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
    conn.execute(
        &format!(
            "UPDATE messages SET embedded = {EMBEDDED_NO_VECTOR}
             WHERE id IN ({placeholders}) AND embedded = 0"
        ),
        params.as_slice(),
    )?;
    Ok(())
}

/// Repair: re-queue every embeddable message that is flagged embedded but has no vector.
/// Older builds marked whole batches embedded when the model failed to load (CWD-relative
/// cache, offline first run), which silently disabled semantic search for those messages.
/// Returns how many were re-queued. Blank messages are re-queued too and simply get marked
/// again on the next pass. Meant to run ONCE (a migration, or behind a one-time flag):
/// running it on every launch would also retry genuinely failing inputs every launch.
pub fn requeue_unvectored(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        &format!(
            "UPDATE messages AS m SET embedded = 0
             WHERE m.embedded = 1 AND {EMBEDDABLE_SQL}
               AND m.id NOT IN (SELECT message_id FROM vec_chunks)"
        ),
        [],
    )?;
    Ok(n)
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
    let embedded = embed_rows(embedder, &rows)?;
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    store_batch(conn, &ids, &embedded.owners, &embedded.vecs)?;
    Ok(rows.len())
}

// ---------------------------------------------------------------------------------------
// Recall facts
// ---------------------------------------------------------------------------------------

/// Embed pending decision/gotcha facts into `vec_facts` for cross-thread recall.
/// Batched with the DB lock released during inference (same discipline as the message
/// embed job). Called after distillation; a thread has only a handful of facts, so this
/// is cheap. Idempotent — only `embedded = 0` facts are processed. A cached-only embedder
/// without a local model leaves the facts pending and returns Ok.
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
        let Some(vecs) = embed_fact_texts(embedder, &rows)? else {
            break;
        };
        // 3. Locked, fast: persist vectors + mark embedded.
        {
            let mut conn = db.0.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
            store_facts(&mut conn, &rows, &vecs)?;
        }
    }
    Ok(())
}

/// Embed all un-embedded recall facts on a RAW connection — for the sidecars (`cal`, MCP)
/// which hold a `Connection`, not the app's `Mutex<Db>`. One pass; small (facts are short).
/// A cached-only embedder without a local model leaves the facts pending and returns Ok;
/// the app embeds them on its next start.
pub fn embed_pending_facts_conn(conn: &mut Connection, embedder: &Embedder) -> Result<()> {
    loop {
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, text FROM facts WHERE embedded = 0 AND kind IN ('decision', 'gotcha') LIMIT 64",
            )?;
            let r = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            r
        };
        if rows.is_empty() {
            break;
        }
        let Some(vecs) = embed_fact_texts(embedder, &rows)? else {
            break;
        };
        store_facts(conn, &rows, &vecs)?;
    }
    Ok(())
}

/// Embed fact texts. `Ok(None)` means a cached-only embedder has no local model: leave
/// the facts pending rather than fail the caller's write (they are embedded later).
fn embed_fact_texts(embedder: &Embedder, rows: &[(i64, String)]) -> Result<Option<Vec<Vec<f32>>>> {
    let texts: Vec<String> = rows.iter().map(|(_, t)| t.clone()).collect();
    match embedder.embed_typed(&texts) {
        Ok(vecs) => Ok(Some(vecs)),
        Err(EmbedError::ModelNotCached(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Persist fact vectors and mark the facts embedded, in one IMMEDIATE transaction.
fn store_facts(conn: &mut Connection, rows: &[(i64, String)], vecs: &[Vec<f32>]) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    {
        let mut ins = tx.prepare("INSERT INTO vec_facts (fact_id, embedding) VALUES (?1, ?2)")?;
        let mut done = tx.prepare("UPDATE facts SET embedded = 1 WHERE id = ?1")?;
        for ((id, _), v) in rows.iter().zip(vecs.iter()) {
            ins.execute(params![id, vec_to_bytes(v)])?;
            done.execute([*id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------

/// Embed a search query (with bge's query instruction prefix) into a single vector.
/// Runs the model — call this WITHOUT holding the DB lock so inference never blocks
/// other UI queries. Loads the model as the embedder allows (the app's `default()`
/// embedder downloads it if missing) and waits for an in-progress load.
///
/// `Ok(None)` for an empty query, or when a `cached_only()` embedder has no local model
/// (callers then run keyword-only). `Err` if loading or inference fails.
pub fn embed_query(embedder: &Embedder, query: &str) -> Result<Option<Vec<f32>>> {
    query_vector(embedder, query, LoadMode::Implicit)
}

/// Like `embed_query`, but NEVER downloads and never waits behind another thread's load,
/// whatever the embedder allows. `Ok(None)` whenever the model isn't usable right now (not
/// downloaded, still loading, or failed to load), so the caller falls back to keyword
/// search; `Err` only when a loaded model fails inference. Use it on interactive paths (UI
/// search, hooks, MCP tools) where a 128 MB download must never happen inline.
pub fn embed_query_cached_only(embedder: &Embedder, query: &str) -> Result<Option<Vec<f32>>> {
    query_vector(embedder, query, LoadMode::CachedNoWait)
}

/// [`embed_query_cached_only`] that says why there is no vector, for the app's search box:
/// it retries soon on `EmbedError::ModelLoading` and shows "unavailable" for the rest.
/// `Ok(None)` only for an empty query.
pub fn embed_query_no_wait(
    embedder: &Embedder,
    query: &str,
) -> std::result::Result<Option<Vec<f32>>, EmbedError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(None);
    }
    let model = embedder.load(LoadMode::CachedNoWait)?;
    embedder.infer_query(&model, query)
}

fn query_vector(embedder: &Embedder, query: &str, mode: LoadMode) -> Result<Option<Vec<f32>>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(None);
    }
    let model = match embedder.load(mode) {
        Ok(model) => model,
        Err(EmbedError::ModelNotCached(_)) => return Ok(None),
        Err(e) if mode == LoadMode::CachedNoWait && e.is_model_unavailable() => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(embedder.infer_query(&model, query)?)
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
    project: Option<&str>,
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    let Some(qv) = embed_query(embedder, query)? else {
        return Ok(Vec::new());
    };
    semantic_search_vec(conn, &qv, include_subagents, sources, project, k)
}

/// SQL-only semantic KNN given an already-embedded query vector. No model inference
/// here, so the DB lock is held only for the (fast) query. See `embed_query`.
pub fn semantic_search_vec(
    conn: &Connection,
    qv: &[f32],
    include_subagents: bool,
    sources: &[String],
    project: Option<&str>,
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    let scope = KnnScope {
        include_subagents,
        sources,
        project,
        ..KnnScope::default()
    };
    semantic_search_scoped(conn, qv, &scope, k)
}

/// The thread and message filters a semantic KNN honors: every `SearchFilters` field that
/// narrows the result set. Hybrid search passes all of them, so Starred, a tag chip or a
/// date range scopes the semantic arm the same way it scopes the keyword arm (it used to
/// apply only project, sources and subagents: `--starred` hybrid returned 24 of 30 hits
/// from unstarred threads).
#[derive(Debug, Clone, Copy)]
pub struct KnnScope<'a> {
    /// Hits below this cosine similarity are dropped: [`SEM_SIMILARITY_FLOOR`] by default
    /// (the search box), [`SEM_RELATED_FLOOR`] for related's code and error context.
    pub floor: f32,
    pub include_subagents: bool,
    pub sources: &'a [String],
    pub project: Option<&'a str>,
    /// How `project` matches (the app's searches are exact, see `search::ProjectMatch`).
    pub project_match: crate::search::ProjectMatch,
    /// Some(true) = only starred threads, Some(false) = only unstarred.
    pub starred: Option<bool>,
    /// Threads having ANY of these tags; empty = all.
    pub tags: &'a [String],
    /// Message timestamp range, epoch seconds, inclusive.
    pub after: Option<i64>,
    pub before: Option<i64>,
}

impl Default for KnnScope<'_> {
    fn default() -> Self {
        Self {
            floor: SEM_SIMILARITY_FLOOR,
            include_subagents: false,
            sources: &[],
            project: None,
            project_match: crate::search::ProjectMatch::Substring,
            starred: None,
            tags: &[],
            after: None,
            before: None,
        }
    }
}

impl<'a> KnnScope<'a> {
    /// The scope for a search's filters, at the default floor.
    pub fn of(filters: &'a crate::search::SearchFilters) -> Self {
        Self {
            floor: SEM_SIMILARITY_FLOOR,
            include_subagents: filters.include_subagents,
            sources: &filters.sources,
            project: filters.project.as_deref(),
            project_match: filters.project_match,
            starred: filters.starred,
            tags: &filters.tags,
            after: filters.after,
            before: filters.before,
        }
    }

    fn project(&self) -> Option<&'a str> {
        self.project.filter(|p| !p.is_empty())
    }

    /// Whether anything narrows the KNN beyond the subagent rule.
    fn is_filtered(&self) -> bool {
        self.project().is_some()
            || !self.sources.is_empty()
            || self.starred.is_some()
            || !self.tags.is_empty()
            || self.after.is_some()
            || self.before.is_some()
    }
}

/// [`semantic_search_vec`] with the full filter scope.
pub fn semantic_search_scoped(
    conn: &Connection,
    qv: &[f32],
    scope: &KnnScope<'_>,
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    let prefilter = scope.is_filtered() && owner_prefilter_usable(conn);
    let (sql, args) = semantic_knn_sql(qv, scope, k, prefilter, vec_partitioned(conn));
    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        let id: i64 = r.get(0)?;
        let dist: f64 = r.get(1)?;
        Ok((id, (1.0 - dist) as f32)) // cosine distance -> similarity
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// sqlite-vec's ceiling on a KNN's `k` (SQLITE_VEC_VEC0_K_MAX); a larger one is an error.
const VEC0_MAX_K: usize = 4096;

/// Whether a scoped KNN can pre-filter through `vec_chunk_owner` without losing vectors.
/// Not when the table is missing: read-only sidecars (`cal search`, hooks) never migrate,
/// so a newer binary can be reading an index from before 0025. Not while some vectors lack
/// an owner row (an older app kept embedding after a newer process migrated the index):
/// the pre-filter would drop them, while the post-filter still finds them. Search runs on
/// read-only connections and can't repair that; `cleanup::reconcile_vec_owners` does, at
/// the end of every embedding build and in Reclaim space. Both checks are per query and
/// cheap (a cached schema lookup, two b-tree counts), so a repair or upgrade that lands
/// under a long-lived pooled reader switches it back to the pre-filter.
fn owner_prefilter_usable(conn: &Connection) -> bool {
    crate::db::has_table(conn, "vec_chunk_owner")
        && !crate::cleanup::vectors_missing_owners(conn).unwrap_or(true)
}

/// The statement and bind values behind `semantic_search_vec`, split out so tests can
/// check its query plan. `owner_prefilter` pushes the thread/message filters into the KNN
/// (see [`owner_prefilter_usable`]); without it they only post-filter an over-fetch.
/// `partitioned`: the table has the subagent partition ([`vec_partitioned`]), so a scope
/// without subagents reads only the main partition.
fn semantic_knn_sql(
    qv: &[f32],
    scope: &KnnScope<'_>,
    k: usize,
    owner_prefilter: bool,
    partitioned: bool,
) -> (String, Vec<Box<dyn ToSql>>) {
    let project = scope.project();
    let sources = scope.sources;
    // Bind order: ?1 the query vector, then each filter's values. Each is bound once and
    // referenced by number from both the pre-filter and the outer query.
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(vec_to_bytes(qv))];
    let mut bind = |v: Box<dyn ToSql>| {
        args.push(v);
        args.len()
    };
    let project_cond = project.map(|p| {
        let ph = bind(crate::search::project_value(p, scope.project_match));
        crate::search::project_condition_at("t", scope.project_match, ph)
    });
    let source_list = sources
        .iter()
        .map(|s| format!("?{}", bind(Box::new(s.clone()))))
        .collect::<Vec<_>>()
        .join(", ");
    let starred_ph = scope.starred.map(|s| bind(Box::new(i64::from(s))));
    let tag_list = scope
        .tags
        .iter()
        .map(|t| format!("?{}", bind(Box::new(t.clone()))))
        .collect::<Vec<_>>()
        .join(", ");
    let after_ph = scope.after.map(|a| bind(Box::new(a)));
    let before_ph = scope.before.map(|b| bind(Box::new(b)));

    // The thread and message conditions (threads `t`, messages `m`), shared by the
    // pre-filter and the outer query. Sources are added per query: they need `sources s`.
    let mut conds = Vec::new();
    if let Some(cond) = project_cond {
        conds.push(cond);
    }
    if let Some(ph) = starred_ph {
        conds.push(format!("t.starred = ?{ph}"));
    }
    if !tag_list.is_empty() {
        conds.push(format!(
            "EXISTS (SELECT 1 FROM thread_tags tt WHERE tt.thread_id = t.id AND tt.tag IN ({tag_list}))"
        ));
    }
    if let Some(ph) = after_ph {
        conds.push(format!("m.ts >= ?{ph}"));
    }
    if let Some(ph) = before_ph {
        conds.push(format!("m.ts <= ?{ph}"));
    }

    // The filters are pushed INTO the KNN as a `rowid IN (...)` pre-filter over the
    // matching threads' chunks, which vec0 serves by skipping every vector outside the
    // list. Post-filtering a global top-k instead starved narrow filters (a project with
    // 0.6% of the vectors got 4-11 of 100 hits; codex, with 3 vectors, got none) despite a
    // 2000-chunk over-fetch. Measured on a 117k-vector index: one project 160 -> 27 ms, a
    // source holding half the vectors 160 -> 80 ms, and every filter gets its full k. A
    // vector with no vec_chunk_owner row would be left out, so the caller only asks for the
    // pre-filter when every vector has one (store_batch writes one per vector).
    let filtered = scope.is_filtered();
    let pushed_down = filtered && owner_prefilter;
    let prefilter = if pushed_down {
        let mut joins = String::from(
            "SELECT o.chunk_rowid FROM vec_chunk_owner o
             JOIN messages m ON m.id = o.message_id
             JOIN threads t ON t.id = m.thread_id",
        );
        let mut pre = conds.clone();
        if !sources.is_empty() {
            joins.push_str(" JOIN sources s ON s.id = t.source_id");
            pre.push(format!("s.kind IN ({source_list})"));
        }
        // Matches the outer filter, shrinks the list, and lets the planner build it from
        // idx_threads_subagent (a few ms per thousand chunks) instead of every owner row.
        if !scope.include_subagents {
            pre.push("t.is_subagent = 0".to_string());
        }
        format!(" AND rowid IN ({joins} WHERE {})", pre.join(" AND "))
    } else {
        String::new()
    };
    // The main partition only, inside the KNN, so subagent vectors never take a slot of the
    // top-k (vec0 skips the other partition's chunks: 276 ms to 115 ms warm for k = 60 on
    // the measured index). It combines with the `rowid IN` list above. The outer
    // `t.is_subagent = 0` stays as a safety net for a vector whose thread changed its flag
    // after it was stored. On the old, unpartitioned shape the outer filter does it alone.
    let partition = if partitioned && !scope.include_subagents {
        " AND subagent = 0"
    } else {
        ""
    };

    // Over-fetch chunks so dedup (several chunks per message) and the remaining
    // post-filters (subagents, legacy tool-call vectors) still leave k messages. A filter
    // that could not be pushed into the KNN post-filters a global top-k, so it fetches far
    // more (a narrow one can still come up short, as before the pre-filter). sqlite-vec
    // requires `k` as a literal (not a bound parameter), so inline our own integer.
    let knn_k = if filtered && !pushed_down {
        (k * 20).max(800)
    } else {
        (k * 5).max(200)
    }
    .min(VEC0_MAX_K);

    // MATERIALIZED so SQLite doesn't inline the KNN into the outer query (vec0 only
    // permits `ORDER BY distance` on the KNN query itself, not the outer ORDER BY).
    // EMBEDDABLE_SQL drops vectors that older builds stored for tool-call rows, so they
    // can't crowd prose out of the results before a migration deletes them. The outer
    // filters repeat the pre-filter's; the subagent one is what scopes an unfiltered search.
    let mut sql = format!(
        "WITH knn AS MATERIALIZED (
            SELECT message_id, distance FROM vec_chunks
            WHERE embedding MATCH ?1 AND k = {knn_k}{partition}{prefilter} ORDER BY distance
         )
         SELECT knn.message_id, MIN(knn.distance) AS d
         FROM knn
         JOIN messages m ON m.id = knn.message_id
         JOIN threads t ON t.id = m.thread_id
         JOIN sources s ON s.id = t.source_id
         WHERE {EMBEDDABLE_SQL}"
    );
    if !scope.include_subagents {
        sql.push_str(" AND t.is_subagent = 0");
    }
    if !sources.is_empty() {
        sql.push_str(&format!(" AND s.kind IN ({source_list})"));
    }
    for c in &conds {
        sql.push_str(" AND ");
        sql.push_str(c);
    }
    // Similarity floor: KNN always returns its k-nearest, so a query with no good semantic
    // match would otherwise inject irrelevant chunks that RRF then promotes. Drop anything
    // below the scope's floor (distance = 1 - cosine).
    args.push(Box::new(1.0_f64 - f64::from(scope.floor)));
    let dist_ph = args.len();
    args.push(Box::new(k as i64));
    let limit_ph = args.len();
    sql.push_str(&format!(
        " GROUP BY knn.message_id HAVING d <= ?{dist_ph} ORDER BY d LIMIT ?{limit_ph}"
    ));
    (sql, args)
}

/// Count of embedded messages vs total embeddable (for UI progress). Both counts use
/// `EMBEDDABLE_SQL`, the same predicate as `pending_batch`, so the bar reaches 100% when
/// the queue is empty instead of counting tool-call rows that are never embedded.
pub fn embed_progress(conn: &Connection) -> Result<(i64, i64)> {
    let done: i64 = conn.query_row(&embedded_count_sql(), [], |r| r.get(0))?;
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages m WHERE {EMBEDDABLE_SQL}"),
        [],
        |r| r.get(0),
    )?;
    Ok((done, total))
}

/// The done half of [`embed_progress`]: with or without a vector ([`EMBEDDED_NO_VECTOR`]).
/// A range on the last column of 0026's covering (role, tool_name, embedded) index.
fn embedded_count_sql() -> String {
    format!("SELECT COUNT(*) FROM messages m WHERE m.embedded > 0 AND {EMBEDDABLE_SQL}")
}

/// Encode an f32 vector as the little-endian byte BLOB sqlite-vec expects.
pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode a vec0 float32 BLOB ([`vec_to_bytes`]'s inverse).
pub fn bytes_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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

    #[test]
    fn chunk_messages_skips_blank_text() {
        let rows = vec![
            (1, "real prose".to_string()),
            (2, "   \n\t".to_string()),
            (3, String::new()),
        ];
        let (owners, texts) = chunk_messages(&rows);
        assert_eq!(owners, vec![1]);
        assert_eq!(texts, vec!["real prose".to_string()]);
    }

    /// The harness's own whole messages get no vector; typed text that only starts like
    /// one, or a slash command with arguments, still does.
    #[test]
    fn chunk_messages_skips_harness_boilerplate() {
        let rows: Vec<(i64, String)> = [
            "<local-command-caveat>Caveat: The messages below were generated by the user \
             while running local commands. DO NOT respond to these messages or otherwise \
             consider them in your response unless the user explicitly asks you to.\
             </local-command-caveat>",
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
            "You've hit your session limit · resets 4:20pm (America/Los_Angeles)",
            "<command-name>/model</command-name>\n<command-message>model</command-message>\n\
             <command-args></command-args>",
            // Kept:
            "<command-name>/plan</command-name>\n<command-message>plan</command-message>\n\
             <command-args>split the vec table by subagent</command-args>",
            "[Request interrupted by user] then: actually, keep the old index for a release",
            "You've hit your limit on retries, so the deploy script should back off",
            "why did the rebuild hold the write lock for 40 s?",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| (i as i64, t.to_string()))
        .collect();
        let (owners, _) = chunk_messages(&rows);
        assert_eq!(owners, vec![5, 6, 7, 8]);
        assert!(!is_embeddable("user", None, &rows[1].1));
        assert!(is_embeddable("user", None, &rows[8].1));
    }

    #[test]
    fn is_embeddable_keeps_prose_only() {
        assert!(is_embeddable("user", None, "how do I rebase"));
        assert!(is_embeddable("assistant", None, "run git rebase -i"));
        // Tool calls are assistant rows with tool_name set.
        assert!(!is_embeddable(
            "assistant",
            Some("Edit"),
            "Edit: {\"file_path\":\"a\"}"
        ));
        assert!(!is_embeddable("tool", None, "exit code 0"));
        assert!(!is_embeddable("tool", Some("Bash"), "ok"));
        assert!(!is_embeddable(
            "system",
            None,
            "you are a helpful assistant"
        ));
        assert!(!is_embeddable("user", None, "  \n "));
    }

    // ---- cache dir ------------------------------------------------------------------

    #[test]
    fn cache_dir_override_wins() {
        let picked = pick_cache_dir(
            Some(PathBuf::from("/custom/models")),
            &[PathBuf::from("/a"), PathBuf::from("/b")],
            |_| true,
        );
        assert_eq!(picked, PathBuf::from("/custom/models"));
    }

    #[test]
    fn cache_dir_takes_first_usable_candidate() {
        let candidates = [
            PathBuf::from("/readonly/models"),
            PathBuf::from("/data/models"),
            PathBuf::from("/tmp/callimachus-models"),
        ];
        let picked = pick_cache_dir(None, &candidates, |p| p != Path::new("/readonly/models"));
        assert_eq!(picked, PathBuf::from("/data/models"));
        // Nothing usable: still the absolute last resort, never a relative path.
        let picked = pick_cache_dir(None, &candidates, |_| false);
        assert_eq!(picked, PathBuf::from("/tmp/callimachus-models"));
    }

    #[test]
    fn default_cache_candidates_are_absolute_app_data() {
        let candidates = cache_dir_candidates();
        assert!(!candidates.is_empty());
        assert!(
            candidates.iter().all(|c| c.is_absolute()),
            "a relative candidate would resolve against the CWD: {candidates:?}"
        );
        // The preferred location is the per-user app-data folder index.db lives in.
        let first = &candidates[0];
        assert!(
            first.ends_with(Path::new(APP_DIR_NAME).join("models")),
            "{first:?}"
        );
    }

    /// A fresh temp dir per test (no external tempfile crate).
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "callimachus_embed_{tag}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Lay out fake model files in hf-hub's cache structure under `root`.
    fn fake_model_cache(root: &Path, skip: Option<&str>) {
        let info = TextEmbedding::get_model_info(&MODEL).unwrap();
        let repo = root.join(format!("models--{}", info.model_code).replace('/', "--"));
        let commit = "0123456789abcdef";
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("refs").join("main"), commit).unwrap();
        let snapshot = repo.join("snapshots").join(commit);
        for f in std::iter::once(info.model_file.as_str()).chain(TOKENIZER_FILES) {
            if Some(f) == skip {
                continue;
            }
            let path = snapshot.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"not a real model").unwrap();
        }
    }

    #[test]
    fn cached_snapshot_requires_every_file() {
        let empty = temp_dir("empty");
        assert!(cached_snapshot(&empty).is_none());

        let complete = temp_dir("complete");
        fake_model_cache(&complete, None);
        let snapshot = cached_snapshot(&complete).expect("complete cache");
        assert!(snapshot.join("tokenizer.json").is_file());

        // An interrupted download (ONNX file never linked into the snapshot) is not cached.
        let partial = temp_dir("partial");
        fake_model_cache(&partial, Some("onnx/model.onnx"));
        assert!(cached_snapshot(&partial).is_none());

        // The lookup takes the first root holding a complete copy, whichever env var (or
        // none) pointed at it.
        let roots = [empty.clone(), partial.clone(), complete.clone()];
        assert_eq!(find_snapshot(&roots), Some(snapshot));
        assert_eq!(find_snapshot(&roots[..2]), None);

        for d in [empty, complete, partial] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Needs a downloaded model (run `embed_smoke` first, or point FASTEMBED_CACHE_DIR at
    /// a cache): the direct snapshot loader must produce exactly fastembed's vectors.
    #[test]
    #[ignore]
    fn snapshot_loader_matches_fastembed() {
        let snapshot = find_snapshot(&lookup_roots()).expect("a cached model");
        // <root>/models--org--name/snapshots/<commit>
        let root = snapshot.ancestors().nth(3).unwrap().to_path_buf();
        let texts = [
            "how do I activate a python virtualenv",
            "sqlite busy timeout",
        ];
        let mut direct = load_snapshot(&snapshot, bulk_intra_threads()).unwrap();
        let mut hub =
            TextEmbedding::try_new(TextInitOptions::new(MODEL).with_cache_dir(root)).unwrap();
        assert_eq!(
            direct.embed(texts, None).unwrap(),
            hub.embed(texts, None).unwrap()
        );
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (norm(a) * norm(b))
    }

    /// Needs a downloaded model. The sidecar's and the background top-ups' small thread
    /// pools must not change vectors.
    #[test]
    #[ignore]
    fn thread_count_does_not_change_vectors() {
        let snapshot = find_snapshot(&lookup_roots()).expect("a cached model");
        let texts = [
            "how do I activate a python virtualenv",
            "sqlite busy timeout",
        ];
        let many = load_snapshot(&snapshot, bulk_intra_threads())
            .unwrap()
            .embed(texts, None)
            .unwrap();
        for threads in [
            1,
            BACKGROUND_SMALL_INTRA_THREADS,
            QUERY_INTRA_THREADS,
            BACKGROUND_INTRA_THREADS,
        ] {
            let few = load_snapshot(&snapshot, threads)
                .unwrap()
                .embed(texts, None)
                .unwrap();
            for (a, b) in few.iter().zip(&many) {
                assert!(cosine(a, b) > 0.99999, "{threads}: cosine {}", cosine(a, b));
            }
        }
    }

    /// Needs a downloaded model. The facts lookup and the hybrid search embed the same
    /// query; the second gets the cached vector, identical to a fresh one, and the cache
    /// stays bounded.
    #[test]
    #[ignore]
    fn a_repeated_query_reuses_its_vector() {
        let e = Embedder::cached_only();
        e.ensure_loaded().unwrap();
        let first = embed_query_cached_only(&e, " pin the rust toolchain ")
            .unwrap()
            .unwrap();
        let again = embed_query_no_wait(&e, "pin the rust toolchain")
            .unwrap()
            .unwrap();
        assert_eq!(first, again);
        let model = e.loaded().unwrap();
        let fresh = e
            .infer_interactive(&model, &[format!("{QUERY_PREFIX}pin the rust toolchain")])
            .unwrap();
        assert!(cosine(&first, &fresh[0]) > 0.99999);
        for i in 0..20 {
            embed_query_cached_only(&e, &format!("query {i}")).unwrap();
        }
        assert_eq!(e.recent_queries.lock().unwrap().len(), QUERY_VECTOR_CACHE);
    }

    /// Needs a downloaded model. Bulk inference runs in length order; every vector must
    /// still land at its input's position. Each text is compared with embedding it alone
    /// (no padding at all), so a mis-permuted output would show up as a low cosine.
    #[test]
    #[ignore]
    fn bulk_length_sort_keeps_input_order() {
        let e = Embedder::cached_only();
        e.ensure_loaded().unwrap();
        let model = e.loaded().unwrap();
        let texts: Vec<String> = (0..(BULK_SUB_BATCH * 2 + 5))
            .map(|i| {
                // Lengths deliberately out of order: long, short, medium, ...
                let words = [40, 2, 15, 7, 60, 1][i % 6];
                format!("note {i} about {}", "sqlite vector search ".repeat(words))
            })
            .collect();
        let bulk = e.embed(texts.clone()).unwrap();
        assert_eq!(bulk.len(), texts.len());
        for (text, got) in texts.iter().zip(&bulk) {
            let alone = e
                .infer_interactive(&model, std::slice::from_ref(text))
                .unwrap();
            let c = cosine(got, &alone[0]);
            assert!(c > 0.99999, "{text:?}: cosine {c}");
        }
    }

    /// Re-embeds chunks that already have vectors in a real index and compares them with
    /// the stored ones. Run it before a fastembed / ort bump: a new runtime that moves vectors
    /// leaves old and new ones in one KNN space, which then needs a full re-embed. Reads an
    /// APFS clone read-only (never the live index) and downloads into FASTEMBED_CACHE_DIR:
    /// `CALLIMACHUS_DB=<clone>/index.db FASTEMBED_CACHE_DIR=<dir> cargo test --release --lib
    /// -- --ignored embedding_drift --nocapture`
    #[test]
    #[ignore]
    fn embedding_drift_against_stored_vectors() {
        let db = std::env::var_os("CALLIMACHUS_DB").expect("CALLIMACHUS_DB = a cloned index");
        let root = env_path(CACHE_DIR_ENV).expect("FASTEMBED_CACHE_DIR = a model dir");
        let want: usize = std::env::var("DRIFT_CHUNKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);
        let conn = crate::db::open_readonly(Path::new(&db)).unwrap();
        // A fixed pseudo-random spread (multiplicative hash of the rowid), so runs on two
        // fastembed versions compare the same chunks.
        let mut sample = conn
            .prepare(
                "SELECT m.id, m.text FROM messages m WHERE m.id IN (
                     SELECT message_id FROM vec_chunk_owner GROUP BY message_id
                     ORDER BY (min(chunk_rowid) * 2654435761) % 4294967296 LIMIT ?1)",
            )
            .unwrap();
        let rows: Vec<(i64, String)> = sample
            .query_map([want as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut owned = conn
            .prepare("SELECT chunk_rowid FROM vec_chunk_owner WHERE message_id = ?1 ORDER BY 1")
            .unwrap();
        let mut stored_vec = conn
            .prepare("SELECT embedding FROM vec_chunks WHERE rowid = ?1")
            .unwrap();
        // store_batch inserts a message's vectors in chunk_text order, so the i-th rowid is
        // the i-th chunk. A count mismatch means the text changed since; skip those.
        let (mut texts, mut stored) = (Vec::new(), Vec::new());
        for (id, text) in &rows {
            let chunks = chunk_text(text);
            let rowids: Vec<i64> = owned
                .query_map([id], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            if chunks.len() != rowids.len() {
                continue;
            }
            for (chunk, rowid) in chunks.into_iter().zip(rowids) {
                let blob: Vec<u8> = stored_vec.query_row([rowid], |r| r.get(0)).unwrap();
                let v: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                texts.push(chunk);
                stored.push(v);
            }
            if texts.len() >= want {
                break;
            }
        }
        assert!(!texts.is_empty(), "no comparable chunks in the sample");

        let e = Embedder::new(true, Some(root), bulk_intra_threads());
        e.ensure_loaded().unwrap();
        let fresh = e.embed(texts.clone()).unwrap();
        let mut scored: Vec<(f32, usize)> = fresh
            .iter()
            .zip(&stored)
            .enumerate()
            .map(|(i, (a, b))| (cosine(a, b), i))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        let median = scored[scored.len() / 2].0;
        println!(
            "drift: {} chunks from {} messages, min {:.6} (1 - min = {:.1e}), median {:.6}",
            scored.len(),
            rows.len(),
            scored[0].0,
            1.0 - scored[0].0,
            median
        );
        for &(c, i) in scored.iter().take(3) {
            println!("  worst {c:.6}: {} chars", texts[i].chars().count());
        }
        assert!(scored[0].0 >= 0.999, "min cosine {} < 0.999", scored[0].0);
    }

    #[test]
    fn lookup_roots_are_absolute_and_include_the_app_dir() {
        let roots = lookup_roots();
        assert!(roots.iter().all(|r| r.is_absolute()), "{roots:?}");
        // The app's own download location is checked whatever HF_HOME says.
        assert!(roots.contains(&cache_dir_candidates()[0]), "{roots:?}");
    }

    // ---- loading policy -------------------------------------------------------------

    #[test]
    fn cached_only_embedder_never_downloads() {
        let root = temp_dir("nocache");
        let e = Embedder::new(false, Some(root.clone()), query_intra_threads());

        // Queries degrade to keyword-only instead of erroring or downloading.
        assert!(embed_query(&e, "how do I activate a venv")
            .unwrap()
            .is_none());
        assert!(embed_query_cached_only(&e, "how do I activate a venv")
            .unwrap()
            .is_none());
        // Bulk paths report the typed, "model unavailable" error.
        let err = e.embed(vec!["text".into()]).unwrap_err();
        assert!(is_model_unavailable(&err), "{err:#}");
        assert!(matches!(
            e.ensure_loaded(),
            Err(EmbedError::ModelNotCached(_))
        ));
        assert!(matches!(
            embed_rows(&e, &[(1, "text".into())]),
            Err(EmbedError::ModelNotCached(_))
        ));
        // Nothing was fetched into the cache dir.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(!e.is_loaded());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_query_skips_the_model() {
        // Even a download-allowed embedder must not load anything for a blank query.
        let e = Embedder::new(true, Some(temp_dir("blank")), bulk_intra_threads());
        assert!(embed_query(&e, "   ").unwrap().is_none());
        assert!(!e.is_loaded());
    }

    #[test]
    fn corrupt_cache_is_model_init_error_with_cooldown() {
        // A cached model is read from its snapshot directly, so HF_HOME can't redirect this.
        let root = temp_dir("corrupt");
        fake_model_cache(&root, None);
        let e = Embedder::new(false, Some(root.clone()), query_intra_threads());

        // Files are "cached", so a load is attempted (offline) and fails on the bad ONNX.
        let first = e.ensure_loaded().unwrap_err();
        assert!(
            matches!(&first, EmbedError::ModelInit(why) if why.contains("bge-small")),
            "{first}"
        );
        assert!(first.is_model_unavailable());

        // Remove the files: a fresh attempt would now say ModelNotCached, so getting the
        // remembered ModelInit back proves implicit loads honor the cooldown.
        std::fs::remove_dir_all(&root).unwrap();
        let again = e.embed(vec!["text".into()]).unwrap_err();
        assert!(
            matches!(
                again.downcast_ref::<EmbedError>(),
                Some(EmbedError::ModelInit(_))
            ),
            "{again:#}"
        );
        assert!(is_model_unavailable(&again));
        // Interactive cached-only queries degrade to keyword search instead of erroring.
        assert!(embed_query_cached_only(&e, "anything").unwrap().is_none());
        // An explicit retry ignores the cooldown and sees the current state.
        assert!(matches!(
            e.ensure_loaded(),
            Err(EmbedError::ModelNotCached(_))
        ));
        assert!(!e.is_loaded());
    }

    #[test]
    fn sidecar_embedder_uses_the_small_thread_pool() {
        let sidecar = Embedder::cached_only();
        assert!((1..=QUERY_INTRA_THREADS).contains(&sidecar.intra_threads));
        assert_eq!(
            sidecar.intra_threads,
            available_cores().min(QUERY_INTRA_THREADS)
        );
        // The app's shared embedder serves queries too; only its build embedder is bulk.
        let app = Embedder::default();
        assert_eq!(app.intra_threads, query_intra_threads());
        assert_eq!(app.bulk_build(None).intra_threads, bulk_intra_threads());
        // A background top-up picks by size: a small one on fewer threads than a backlog,
        // both under the bulk pool on a machine with cores to spare.
        let small = app.bulk_build(Some(40)).intra_threads;
        let backlog = app.bulk_build(Some(2_000)).intra_threads;
        assert_eq!(
            small,
            bulk_intra_threads().min(BACKGROUND_SMALL_INTRA_THREADS)
        );
        assert_eq!(backlog, bulk_intra_threads().min(BACKGROUND_INTRA_THREADS));
        assert!(small <= backlog && backlog <= bulk_intra_threads());
        assert_eq!(
            app.bulk_build(Some(BACKGROUND_SMALL_MESSAGES))
                .intra_threads,
            small
        );
    }

    #[test]
    fn idle_unload_policy_per_embedder() {
        assert_eq!(Embedder::default().idle_unload, Some(APP_IDLE_UNLOAD));
        assert_eq!(
            Embedder::cached_only().idle_unload,
            Some(SIDECAR_IDLE_UNLOAD)
        );
        let app = Embedder::default();
        let build = app.bulk_build(None);
        // The build owns its model; nothing reaps it mid-build.
        assert_eq!(build.idle_unload, None);
        assert!(!build.unload_if_idle());
        // A query on the shared embedder still makes the build's bulk loop step aside.
        let _ticket = InteractiveTicket::enter(&app.interactive);
        assert_eq!(build.interactive.load(Ordering::Acquire), 1);
        // Nothing loaded: nothing to unload.
        assert!(!app.unload_if_idle());
        assert!(!app.is_loaded());
    }

    /// Needs a downloaded model. The model unloads only once idle past the limit and only
    /// while no call holds it, and the next query reloads it.
    #[test]
    #[ignore]
    fn idle_model_unloads_and_reloads() {
        let e = Embedder::cached_only();
        e.ensure_loaded().unwrap();
        assert!(!e.unload_if_idle(), "used just now");
        let held = e.loaded().unwrap();
        assert!(!e.unload_idle_for(Duration::ZERO), "a call still holds it");
        drop(held);
        let started = Instant::now();
        assert!(e.unload_idle_for(Duration::ZERO));
        assert!(!e.is_loaded());
        let qv = embed_query_cached_only(&e, "sqlite vector index").unwrap();
        assert_eq!(qv.map(|v| v.len()), Some(DIM));
        assert!(e.is_loaded());
        eprintln!("unload + reload + one query: {:?}", started.elapsed());
    }

    #[test]
    fn length_order_is_shortest_first_and_stable() {
        let texts: Vec<String> = ["ccc", "a", "bb", "x", "dddd", ""]
            .into_iter()
            .map(String::from)
            .collect();
        // Equal lengths ("a", "x") keep arrival order.
        assert_eq!(length_order(&texts), vec![5, 1, 3, 2, 0, 4]);
        assert!(length_order(&[]).is_empty());
    }

    #[test]
    fn interactive_ticket_counts_and_releases() {
        let counter = AtomicUsize::new(0);
        {
            let _a = InteractiveTicket::enter(&counter);
            let _b = InteractiveTicket::enter(&counter);
            assert_eq!(counter.load(Ordering::Acquire), 2);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    // ---- queue predicate, progress, repair ------------------------------------------

    struct TempDb {
        path: PathBuf,
        conn: Connection,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.path.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn temp_db(tag: &str) -> TempDb {
        let path = temp_dir(tag).join("index.db");
        let conn = crate::db::open(&path).unwrap();
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title) VALUES (1, ?1, 't')",
            [tag],
        )
        .unwrap();
        TempDb { path, conn }
    }

    fn insert_msg(conn: &Connection, seq: i64, role: &str, text: &str, tool: Option<&str>) -> i64 {
        let tid: i64 = conn
            .query_row("SELECT MAX(id) FROM threads", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO messages (thread_id, seq, role, text, tool_name) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![tid, seq, role, text, tool],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn pending_and_progress_skip_tool_calls() {
        let mut db = temp_db("pending");
        let conn = &db.conn;
        let user = insert_msg(conn, 0, "user", "how do I activate a venv", None);
        let asst = insert_msg(conn, 1, "assistant", "source .venv/bin/activate", None);
        let _call = insert_msg(
            conn,
            2,
            "assistant",
            "Bash: {\"command\":\"ls\"}",
            Some("Bash"),
        );
        let _out = insert_msg(conn, 3, "tool", "README.md", None);
        let blank = insert_msg(conn, 4, "user", "   ", None);

        // The SQL predicate and the Rust predicate agree (modulo the blank-text check).
        let sql_ids: Vec<i64> = conn
            .prepare(&format!(
                "SELECT m.id FROM messages m WHERE {EMBEDDABLE_SQL} ORDER BY m.id"
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(sql_ids, vec![user, asst, blank]);

        let mut pending: Vec<i64> = pending_batch(conn, 100)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        pending.sort_unstable();
        assert_eq!(pending, vec![user, asst, blank], "tool rows never queue");
        assert_eq!(embed_progress(conn).unwrap(), (0, 3));

        // Store fake vectors for the batch: the blank row gets no chunk but is marked done.
        let rows = pending_batch(conn, 100).unwrap();
        let (owners, texts) = chunk_messages(&rows);
        assert_eq!(texts.len(), 2);
        let vecs = vec![vec![0.1_f32; DIM]; texts.len()];
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        store_batch(&mut db.conn, &ids, &owners, &vecs).unwrap();

        let conn = &db.conn;
        assert!(pending_batch(conn, 100).unwrap().is_empty());
        assert_eq!(
            embed_progress(conn).unwrap(),
            (3, 3),
            "no phantom work left"
        );
        let state = |id: i64| -> i64 {
            conn.query_row("SELECT embedded FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!((state(user), state(asst)), (1, 1));
        assert_eq!(state(blank), EMBEDDED_NO_VECTOR, "done, without a vector");
    }

    /// The done count reads 0026's covering index (a range on `embedded`), never the rows.
    #[test]
    fn embedded_count_reads_the_covering_index() {
        let db = temp_db("donecount");
        let plan: Vec<String> = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", embedded_count_sql()))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|l| l.contains("COVERING INDEX idx_messages_embed_counts")
                    && l.contains("embedded>?")),
            "{plan:?}"
        );
    }

    /// A boilerplate (or blank, or failed) message has no vector and no owner row. Marked
    /// embedded = 1 it met 0025's fallback trigger, whose `DELETE FROM vec_chunks WHERE
    /// message_id = ...` scans every vector: so its delete must cost the same VM steps
    /// however many vectors the table holds, while a message with vectors still loses them.
    #[test]
    fn deleting_a_vectorless_message_never_scans_the_vectors() {
        use rusqlite::StatementStatus;
        let mut db = temp_db("vectorless");
        let embed_all = |db: &mut TempDb| {
            let rows = pending_batch(&db.conn, 10_000).unwrap();
            let (owners, texts) = chunk_messages(&rows);
            let vecs: Vec<Vec<f32>> = (0..texts.len())
                .map(|i| {
                    let mut v = vec![0.0_f32; DIM];
                    v[i % DIM] = 1.0;
                    v
                })
                .collect();
            let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            store_batch(&mut db.conn, &ids, &owners, &vecs).unwrap();
        };
        let boilerplate = |db: &TempDb, seq: i64| {
            insert_msg(&db.conn, seq, "user", "[Request interrupted by user]", None)
        };
        let first = boilerplate(&db, 0);
        let prose = insert_msg(&db.conn, 1, "assistant", "rebuild the vec table", None);
        embed_all(&mut db);
        let delete_steps = |db: &TempDb, id: i64| -> i32 {
            let mut st = db
                .conn
                .prepare("DELETE FROM messages WHERE id = ?1")
                .unwrap();
            st.execute([id]).unwrap();
            st.get_status(StatementStatus::VmStep)
        };
        let before = delete_steps(&db, first);
        // Two thousand more vectors, and another boilerplate message.
        for seq in 2..2_002 {
            insert_msg(
                &db.conn,
                seq,
                "user",
                &format!("message number {seq}"),
                None,
            );
        }
        let second = boilerplate(&db, 5_000);
        embed_all(&mut db);
        let vectors = |db: &TempDb| -> i64 {
            db.conn
                .query_row("SELECT COUNT(*) FROM vec_chunks", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(vectors(&db), 2_001);
        assert_eq!(delete_steps(&db, second), before, "no vector scan");
        assert_eq!(vectors(&db), 2_001);
        delete_steps(&db, prose);
        assert_eq!(vectors(&db), 2_000, "a message's own vectors still go");
    }

    #[test]
    fn requeue_unvectored_repairs_vectorless_rows() {
        let mut db = temp_db("requeue");
        let conn = &db.conn;
        let with_vec = insert_msg(conn, 0, "user", "has a vector", None);
        let lost = insert_msg(conn, 1, "assistant", "marked without a vector", None);
        let tool = insert_msg(conn, 2, "assistant", "Edit: {}", Some("Edit"));
        let still_pending = insert_msg(conn, 3, "user", "not embedded yet", None);

        store_batch(
            &mut db.conn,
            &[with_vec],
            &[with_vec],
            &[vec![0.2_f32; DIM]],
        )
        .unwrap();
        let conn = &db.conn;
        // What the old job did when the model failed to load.
        conn.execute(
            "UPDATE messages SET embedded = 1 WHERE id IN (?1, ?2)",
            [lost, tool],
        )
        .unwrap();
        // An input that failed inference today: done on purpose, never re-queued.
        let failed = insert_msg(conn, 4, "user", "failed inference", None);
        mark_embedded(conn, &[failed]).unwrap();

        assert_eq!(
            requeue_unvectored(conn).unwrap(),
            1,
            "only the lost prose row"
        );
        let mut pending: Vec<i64> = pending_batch(conn, 100)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        pending.sort_unstable();
        assert_eq!(pending, vec![lost, still_pending]);
        // Idempotent.
        assert_eq!(requeue_unvectored(conn).unwrap(), 0);
    }

    #[test]
    fn semantic_search_ignores_tool_call_vectors() {
        let mut db = temp_db("semtool");
        let conn = &db.conn;
        let prose = insert_msg(conn, 0, "assistant", "activate the venv", None);
        let call = insert_msg(
            conn,
            1,
            "assistant",
            "Bash: source .venv/bin/activate",
            Some("Bash"),
        );
        // An older build embedded tool calls; both rows carry the same vector here.
        let v = vec![0.3_f32; DIM];
        store_batch(
            &mut db.conn,
            &[prose, call],
            &[prose, call],
            &[v.clone(), v.clone()],
        )
        .unwrap();

        let hits = semantic_search_vec(&db.conn, &v, true, &[], None, 10).unwrap();
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![prose]);
    }

    /// A new thread (the target of later `insert_msg` calls). Source 1 is claude_code and
    /// 2 is codex in the seeded `sources` table.
    fn insert_thread(conn: &Connection, ext: &str, project: &str, source_id: i64, subagent: bool) {
        conn.execute(
            "INSERT INTO threads (source_id, external_id, title, project_path, is_subagent)
             VALUES (?1, ?2, 't', ?3, ?4)",
            params![source_id, ext, project, subagent as i64],
        )
        .unwrap();
    }

    /// A vector at a known cosine from the query `direction(0.0)`: cos = 1/sqrt(1 + x^2).
    fn direction(x: f32) -> Vec<f32> {
        let mut v = vec![0.0_f32; DIM];
        v[0] = 1.0;
        v[1] = x;
        v
    }

    #[test]
    fn project_and_source_filters_prefilter_the_knn() {
        let mut db = temp_db("semproj");
        // 900 claude_code vectors in one project, all nearer the query than anything else
        // and more than any over-fetch: a post-filtered global top-k finds nothing outside.
        insert_thread(&db.conn, "big", "/w/big", 1, false);
        let big: Vec<i64> = (0..900)
            .map(|i| insert_msg(&db.conn, i, "user", "big", None))
            .collect();
        insert_thread(&db.conn, "small", "/w/small", 1, false);
        let small = insert_msg(&db.conn, 0, "assistant", "small", None);
        insert_thread(&db.conn, "small-codex", "/w/small", 2, false);
        let small_codex = insert_msg(&db.conn, 0, "user", "small, codex", None);
        insert_thread(&db.conn, "small-sub", "/w/small", 1, true);
        let small_sub = insert_msg(&db.conn, 0, "user", "small, subagent", None);

        let mut ids = big.clone();
        let mut vecs = vec![direction(0.05); big.len()];
        ids.extend([small, small_codex, small_sub]);
        vecs.extend([direction(0.5), direction(0.6), direction(0.4)]);
        store_batch(&mut db.conn, &ids, &ids, &vecs).unwrap();

        let q = direction(0.0);
        let search = |subagents: bool, sources: &[&str], project: Option<&str>, k: usize| {
            let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
            semantic_search_vec(&db.conn, &q, subagents, &sources, project, k)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<i64>>()
        };

        // Nearest first, subagent thread hidden by default.
        assert_eq!(
            search(false, &[], Some("small"), 10),
            vec![small, small_codex]
        );
        assert_eq!(search(false, &[], Some("small"), 1), vec![small]);
        assert_eq!(
            search(true, &[], Some("small"), 10),
            vec![small_sub, small, small_codex]
        );
        // Source kinds apply inside the pre-filter, with or without a project.
        assert_eq!(
            search(false, &["codex"], Some("small"), 10),
            vec![small_codex]
        );
        assert_eq!(search(false, &["codex"], None, 10), vec![small_codex]);
        assert_eq!(
            search(true, &["claude_code"], Some("small"), 10),
            vec![small_sub, small]
        );
        assert!(search(false, &["cursor"], None, 10).is_empty());
        // A broad pattern (the list holds every vector) still ranks globally.
        let broad = search(false, &[], Some("/w/"), 5);
        assert_eq!(broad.len(), 5);
        assert!(broad.iter().all(|id| big.contains(id)), "{broad:?}");
        // Unscoped: the big project's nearer vectors win, as before.
        let all = search(false, &[], None, 5);
        assert_eq!(all.len(), 5);
        assert!(all.iter().all(|id| big.contains(id)), "{all:?}");
    }

    /// EXPLAIN QUERY PLAN detail lines for the `semantic_search_vec` statement.
    fn semantic_plan(
        conn: &Connection,
        include_subagents: bool,
        sources: &[String],
        project: Option<&str>,
        owner_prefilter: bool,
        partitioned: bool,
    ) -> Vec<String> {
        let scope = KnnScope {
            include_subagents,
            sources,
            project,
            ..KnnScope::default()
        };
        let (sql, args) =
            semantic_knn_sql(&direction(0.0), &scope, 10, owner_prefilter, partitioned);
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map(refs.as_slice(), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn filtered_search_plan_uses_vec0_rowid_in() {
        let db = temp_db("semplan");
        let codex = vec!["codex".to_string()];
        for include_subagents in [false, true] {
            for (sources, project) in [
                (&codex, None),
                (&vec![], Some("cal")),
                (&codex, Some("cal")),
            ] {
                let plan =
                    semantic_plan(&db.conn, include_subagents, sources, project, true, false);
                // vec0 marks a consumed `rowid IN` with '[' in its index string. Without
                // it the list would be checked after a global top-k: starvation again.
                assert!(
                    plan.iter()
                        .any(|l| l.contains("VIRTUAL TABLE INDEX") && l.contains('[')),
                    "{plan:#?}"
                );
                // Building the list must never walk the whole messages table.
                assert!(!plan.iter().any(|l| l.starts_with("SCAN m")), "{plan:#?}");
                // The fallback never touches the owner map.
                let plan =
                    semantic_plan(&db.conn, include_subagents, sources, project, false, false);
                assert!(!plan.iter().any(|l| l.contains('[')), "{plan:#?}");
                assert!(
                    !plan.iter().any(|l| l.contains("vec_chunk_owner")),
                    "{plan:#?}"
                );
            }
        }
        // An unfiltered search has no list to build.
        for include_subagents in [false, true] {
            let plan = semantic_plan(&db.conn, include_subagents, &[], None, true, false);
            assert!(!plan.iter().any(|l| l.contains('[')), "{plan:#?}");
        }
    }

    /// Starred, tag, date and exact-project scopes ride the same `rowid IN` pre-filter, and building
    /// their list never walks the messages table either.
    #[test]
    fn collection_and_date_filters_prefilter_the_knn() {
        let db = temp_db("semplan2");
        let tags = vec!["infra".to_string()];
        let scopes = [
            KnnScope {
                starred: Some(true),
                ..KnnScope::default()
            },
            KnnScope {
                tags: &tags,
                ..KnnScope::default()
            },
            KnnScope {
                after: Some(1),
                before: Some(2),
                ..KnnScope::default()
            },
            // The app's exact project key rides the same pre-filter.
            KnnScope {
                project: Some("/r/cal"),
                project_match: crate::search::ProjectMatch::Exact,
                ..KnnScope::default()
            },
        ];
        for scope in &scopes {
            let (sql, args) = semantic_knn_sql(&direction(0.0), scope, 10, true, false);
            let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let plan: Vec<String> = db
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(refs.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|l| l.contains("VIRTUAL TABLE INDEX") && l.contains('[')),
                "{scope:?}: {plan:#?}"
            );
            assert!(
                !plan.iter().any(|l| l.starts_with("SCAN m")),
                "{scope:?}: {plan:#?}"
            );
        }
    }

    #[test]
    fn knn_k_fits_vec0_and_over_fetches_without_the_prefilter() {
        let q = direction(0.0);
        let codex = vec!["codex".to_string()];
        let knn_k = |sources: &[String], prefilter: bool, k: usize| {
            let scope = KnnScope {
                sources,
                ..KnnScope::default()
            };
            let (sql, _) = semantic_knn_sql(&q, &scope, k, prefilter, false);
            let tail = &sql[sql.find("AND k = ").unwrap() + "AND k = ".len()..];
            tail.split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
        };
        assert_eq!(knn_k(&[], true, 10), 200);
        assert_eq!(
            knn_k(&codex, true, 10),
            200,
            "pre-filtered: the list does the work"
        );
        assert_eq!(knn_k(&codex, false, 10), 800, "post-filtered: over-fetch");
        assert_eq!(knn_k(&codex, false, 1000), VEC0_MAX_K);
        assert_eq!(knn_k(&[], true, 1000), VEC0_MAX_K);
    }

    /// Scoped search must not lose vectors the owner map can't vouch for: ones an older
    /// app wrote without an owner row, or every vector on an index from before 0025 (read
    /// by a sidecar that never migrates). Both fall back to post-filtering.
    #[test]
    fn scoped_search_falls_back_when_the_owner_map_cannot_serve_it() {
        let mut db = temp_db("semfallback");
        insert_thread(&db.conn, "a", "/w/alpha", 1, false);
        let alpha = insert_msg(&db.conn, 0, "user", "alpha", None);
        insert_thread(&db.conn, "b", "/w/beta", 2, false);
        let beta = insert_msg(&db.conn, 0, "user", "beta", None);
        store_batch(
            &mut db.conn,
            &[alpha, beta],
            &[alpha, beta],
            &[direction(0.1), direction(0.2)],
        )
        .unwrap();
        // An older app (no owner map) embeds after a newer process migrated the index.
        insert_thread(&db.conn, "c", "/w/alpha", 1, false);
        let unowned = insert_msg(&db.conn, 0, "assistant", "alpha, unowned", None);
        db.conn
            .execute(
                "INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)",
                params![unowned, vec_to_bytes(&direction(0.3))],
            )
            .unwrap();
        db.conn
            .execute("UPDATE messages SET embedded = 1 WHERE id = ?1", [unowned])
            .unwrap();

        let q = direction(0.0);
        let search = |conn: &Connection, sources: &[&str], project: Option<&str>| {
            let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
            semantic_search_vec(conn, &q, false, &sources, project, 10)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<i64>>()
        };

        assert!(!owner_prefilter_usable(&db.conn), "an unowned vector");
        assert_eq!(search(&db.conn, &[], Some("alpha")), vec![alpha, unowned]);
        assert_eq!(
            search(&db.conn, &["claude_code"], None),
            vec![alpha, unowned]
        );

        // Repaired: back on the pre-filter, same answers.
        assert_eq!(crate::cleanup::reconcile_vec_owners(&db.conn).unwrap(), 1);
        assert!(owner_prefilter_usable(&db.conn));
        assert_eq!(search(&db.conn, &[], Some("alpha")), vec![alpha, unowned]);
        assert_eq!(
            search(&db.conn, &["claude_code"], None),
            vec![alpha, unowned]
        );

        // No owner map at all: a hard "no such table" before this fallback.
        db.conn.execute_batch("DROP TABLE vec_chunk_owner").unwrap();
        assert!(!owner_prefilter_usable(&db.conn));
        assert_eq!(search(&db.conn, &[], Some("alpha")), vec![alpha, unowned]);
        assert_eq!(search(&db.conn, &["codex"], Some("beta")), vec![beta]);
        assert_eq!(search(&db.conn, &["codex"], None), vec![beta]);
        assert!(search(&db.conn, &["cursor"], Some("alpha")).is_empty());
    }

    // ---- subagent partition (0036) ----------------------------------------------------

    /// Thread ids of the default-scope hits.
    fn hit_threads(conn: &Connection, hits: &[(i64, f32)]) -> Vec<i64> {
        hits.iter()
            .map(|(mid, _)| {
                conn.query_row("SELECT thread_id FROM messages WHERE id = ?1", [mid], |r| {
                    r.get(0)
                })
                .unwrap()
            })
            .collect()
    }

    /// 1,000 subagent chunks nearer the query than any of 50 main-thread chunks: the
    /// default scope must still return min(k, 50) main hits. On the old shape the global
    /// top-k (200 for k <= 40) is all subagent and the post-filter leaves nothing, which is
    /// what a default search on a subagent-heavy index did before the partition.
    #[test]
    fn subagent_vectors_never_crowd_out_main_hits() {
        let mut db = temp_db("crowding");
        insert_thread(&db.conn, "sub", "/w/p", 1, true);
        let sub: Vec<i64> = (0..1000)
            .map(|i| insert_msg(&db.conn, i, "assistant", "subagent echo", None))
            .collect();
        insert_thread(&db.conn, "main", "/w/p", 1, false);
        let main_tid: i64 = db
            .conn
            .query_row("SELECT MAX(id) FROM threads", [], |r| r.get(0))
            .unwrap();
        let main: Vec<i64> = (0..50)
            .map(|i| insert_msg(&db.conn, i, "user", "main prose", None))
            .collect();
        // Subagent vectors at cosine 0.995..0.99, main ones at 0.98..0.83: all above the
        // floor, every subagent one nearer.
        let mut ids = sub.clone();
        let mut vecs: Vec<Vec<f32>> = (0..sub.len())
            .map(|i| direction(0.1 + i as f32 * 0.00004))
            .collect();
        ids.extend(&main);
        vecs.extend((0..main.len()).map(|i| direction(0.2 + i as f32 * 0.012)));
        store_batch(&mut db.conn, &ids, &ids, &vecs).unwrap();

        let q = direction(0.0);
        let default = |conn: &Connection, k: usize| {
            semantic_search_vec(conn, &q, false, &[], None, k).unwrap()
        };
        assert!(!vec_partitioned(&db.conn));
        assert!(
            default(&db.conn, 10).is_empty(),
            "the old shape: crowded out"
        );

        let report = crate::cleanup::rebuild_vec_chunks(&mut db.conn)
            .unwrap()
            .expect("the old shape is rebuilt");
        assert!(report.partitioned);
        assert_eq!(report.vectors, 1050);
        assert!(vec_partitioned(&db.conn));
        for (k, want) in [(10, 10), (50, 50), (60, 50)] {
            let hits = default(&db.conn, k);
            assert_eq!(hits.len(), want, "k = {k}");
            assert!(hit_threads(&db.conn, &hits).iter().all(|t| *t == main_tid));
            // Nearest first, as before.
            let got: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            assert_eq!(got, main[..want].to_vec());
        }
        // include_subagents reads both partitions: the subagent vectors win, as they did.
        let all = semantic_search_vec(&db.conn, &q, true, &[], None, 10).unwrap();
        assert_eq!(all.len(), 10);
        assert!(all.iter().all(|(id, _)| sub.contains(id)), "{all:?}");
        // A scoped search combines the partition with the `rowid IN` pre-filter.
        let scoped = semantic_search_vec(&db.conn, &q, false, &[], Some("/w/p"), 5).unwrap();
        let got: Vec<i64> = scoped.iter().map(|(id, _)| *id).collect();
        assert_eq!(got, main[..5].to_vec());

        // New vectors land in their thread's partition.
        insert_thread(&db.conn, "main2", "/w/q", 1, false);
        let late = insert_msg(&db.conn, 0, "user", "late main prose", None);
        insert_thread(&db.conn, "sub2", "/w/q", 1, true);
        let late_sub = insert_msg(&db.conn, 0, "user", "late subagent", None);
        store_batch(
            &mut db.conn,
            &[late, late_sub],
            &[late, late_sub],
            &[direction(0.0), direction(0.0)],
        )
        .unwrap();
        let partition = |mid: i64| -> i64 {
            db.conn
                .query_row(
                    "SELECT subagent FROM vec_chunks WHERE rowid =
                         (SELECT chunk_rowid FROM vec_chunk_owner WHERE message_id = ?1)",
                    [mid],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!((partition(late), partition(late_sub)), (0, 1));
        assert_eq!(default(&db.conn, 1)[0].0, late);
        // No NULL partition ever: each would be a chunk of its own that no KNN reads.
        let null_chunks: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM vec_chunks_chunks WHERE partition00 IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_chunks, 0);
    }

    /// Both shapes give the same answers wherever the old one was not crowded out: a pooled
    /// reader or a sidecar may meet either while the rebuild is pending.
    #[test]
    fn old_and_partitioned_shapes_answer_alike() {
        let mut db = temp_db("shapes");
        let mut ids = Vec::new();
        let mut vecs = Vec::new();
        for (t, (project, source, subagent)) in [
            ("/w/a", 1, false),
            ("/w/a", 1, true),
            ("/w/b", 2, false),
            ("/w/b", 1, true),
        ]
        .into_iter()
        .enumerate()
        {
            insert_thread(&db.conn, &format!("t{t}"), project, source, subagent);
            for i in 0..5 {
                ids.push(insert_msg(&db.conn, i, "user", "x", None));
                vecs.push(direction(0.05 * (1 + t * 5 + i as usize) as f32));
            }
        }
        store_batch(&mut db.conn, &ids, &ids, &vecs).unwrap();
        let q = direction(0.0);
        let codex = ["codex".to_string()];
        let answers = |conn: &Connection| {
            let mut out = Vec::new();
            for subagents in [false, true] {
                for (sources, project) in [
                    (&[][..], None),
                    (&[][..], Some("/w/a")),
                    (&codex[..], None),
                    (&[][..], Some("/w/b")),
                ] {
                    out.push(
                        semantic_search_vec(conn, &q, subagents, sources, project, 7)
                            .unwrap()
                            .into_iter()
                            .map(|(id, _)| id)
                            .collect::<Vec<i64>>(),
                    );
                }
            }
            out
        };
        let before = answers(&db.conn);
        assert!(before.iter().all(|a| !a.is_empty()), "{before:?}");
        crate::cleanup::rebuild_vec_chunks(&mut db.conn).unwrap();
        assert!(vec_partitioned(&db.conn));
        assert_eq!(answers(&db.conn), before);
    }

    /// The default scope pushes `subagent = 0` into vec0 (']Aa' in its index string: a
    /// partition constraint, EQ, on partition column A), alone and next to the `rowid IN`
    /// pre-filter; include_subagents and the old shape never add it.
    #[test]
    fn partitioned_plan_reads_the_main_partition_only() {
        let mut db = temp_db("partplan");
        crate::cleanup::rebuild_vec_chunks(&mut db.conn).unwrap();
        assert!(
            vec_partitioned(&db.conn),
            "an empty table is partitioned at once"
        );
        let codex = vec!["codex".to_string()];
        for (sources, project) in [(&vec![], None), (&codex, None), (&vec![], Some("cal"))] {
            let plan = semantic_plan(&db.conn, false, sources, project, true, true);
            assert!(plan.iter().any(|l| l.contains("]Aa")), "{plan:#?}");
            let filtered = !sources.is_empty() || project.is_some();
            assert_eq!(
                plan.iter()
                    .any(|l| l.contains("VIRTUAL TABLE INDEX") && l.contains('[')),
                filtered,
                "{plan:#?}"
            );
            let plan = semantic_plan(&db.conn, true, sources, project, true, true);
            assert!(!plan.iter().any(|l| l.contains("]A")), "{plan:#?}");
        }
        let fresh = temp_db("partplan-old");
        let plan = semantic_plan(&fresh.conn, false, &[], None, true, false);
        assert!(!plan.iter().any(|l| l.contains("]A")), "{plan:#?}");
    }

    /// Default-scope KNN latency and main-thread hits on a real index, before and after its
    /// 0036 partition. Read-only on an APFS clone (never the live index); no model: the five
    /// query vectors are stored chunks picked by a fixed hash of their rowid, which the
    /// rebuild keeps, so a run before and one after compare the same queries.
    /// `CALLIMACHUS_DB=<clone>/index.db cargo test --release -p callimachus-core --lib --
    /// --ignored real_index_default_knn --nocapture`
    #[test]
    #[ignore]
    fn real_index_default_knn() {
        let db = std::env::var_os("CALLIMACHUS_DB").expect("CALLIMACHUS_DB = a cloned index");
        let conn = crate::db::open_readonly(Path::new(&db)).unwrap();
        let picks: Vec<(i64, i64)> = conn
            .prepare(
                "SELECT o.chunk_rowid, t.is_subagent FROM vec_chunk_owner o
                 JOIN messages m ON m.id = o.message_id JOIN threads t ON t.id = m.thread_id
                 ORDER BY (o.chunk_rowid * 2654435761) % 4294967296 LIMIT 5",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        println!(
            "partitioned: {}, vectors: {}",
            vec_partitioned(&conn),
            conn.query_row("SELECT COUNT(*) FROM vec_chunks_rowids", [], |r| r
                .get::<_, i64>(0))
                .unwrap()
        );
        for (rowid, subagent) in picks {
            let blob: Vec<u8> = conn
                .query_row(
                    "SELECT embedding FROM vec_chunks WHERE rowid = ?1",
                    [rowid],
                    |r| r.get(0),
                )
                .unwrap();
            let qv = bytes_to_vec(&blob);
            // Hybrid's semantic arm asks for SEM_ARM_MAX (60) under the default scope.
            let scope = KnnScope::default();
            let t = Instant::now();
            let hits = semantic_search_scoped(&conn, &qv, &scope, 60).unwrap();
            let first = t.elapsed();
            let mut warm: Vec<Duration> = (0..5)
                .map(|_| {
                    let t = Instant::now();
                    semantic_search_scoped(&conn, &qv, &scope, 60).unwrap();
                    t.elapsed()
                })
                .collect();
            warm.sort();
            println!(
                "query chunk {rowid} (subagent {subagent}): {} main hits, first {:.1} ms, warm median {:.1} ms",
                hits.len(),
                first.as_secs_f64() * 1000.0,
                warm[2].as_secs_f64() * 1000.0
            );
        }
    }

    /// The most repeated chunk texts of a real index's embedded messages, with how much of
    /// the vector table is repeats. Read-only on an APFS clone; no model.
    /// `CALLIMACHUS_DB=<clone>/index.db cargo test --release -p callimachus-core --lib --
    /// --ignored real_index_repeated_chunks --nocapture`
    #[test]
    #[ignore]
    fn real_index_repeated_chunks() {
        let db = std::env::var_os("CALLIMACHUS_DB").expect("CALLIMACHUS_DB = a cloned index");
        let conn = crate::db::open_readonly(Path::new(&db)).unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT m.id, m.text, t.is_subagent FROM messages m
                 JOIN threads t ON t.id = m.thread_id
                 WHERE m.embedded = 1 AND {EMBEDDABLE_SQL}"
            ))
            .unwrap();
        // key -> (chunks, of them in subagent threads, the text's first 80 chars)
        let mut seen: std::collections::HashMap<ChunkKey, (u32, u32, String)> =
            std::collections::HashMap::new();
        let mut total = 0usize;
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let row = (r.get::<_, i64>(0).unwrap(), r.get::<_, String>(1).unwrap());
            let sub = r.get::<_, i64>(2).unwrap() != 0;
            for text in chunk_messages(std::slice::from_ref(&row)).1 {
                total += 1;
                let e = seen
                    .entry(ChunkKey::of(&text))
                    .or_insert_with(|| (0, 0, text.chars().take(80).collect()));
                e.0 += 1;
                e.1 += u32::from(sub);
            }
        }
        let repeats: usize = seen.values().map(|e| e.0 as usize - 1).sum();
        println!(
            "{total} chunks, {} distinct, {repeats} repeats ({:.1}%)",
            seen.len(),
            100.0 * repeats as f64 / total.max(1) as f64
        );
        let mut top: Vec<_> = seen.into_values().filter(|e| e.0 > 1).collect();
        top.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (n, sub, head) in top.iter().take(20) {
            let head = crate::context::redact_secrets(head).replace(['\n', '\r'], " ");
            println!("{n:>6} ({sub:>6} subagent)  {head}");
        }
    }

    /// Chunks per CPU-second of a background build at 1, 2, 4 and 8 ONNX threads, on a
    /// small top-up and on a large batch of real chunk texts, through the bulk path a
    /// build uses (128-message batches, 16-chunk length-sorted sub-batches). Needs the
    /// cached model (never downloads) and an APFS clone, read-only:
    /// `CALLIMACHUS_DB=<clone>/index.db FASTEMBED_CACHE_DIR=<dir> cargo test --release -p
    /// callimachus-core --lib -- --ignored background_threads_bench --nocapture
    /// --test-threads 1`
    #[test]
    #[ignore]
    fn background_threads_bench() {
        let db = std::env::var_os("CALLIMACHUS_DB").expect("CALLIMACHUS_DB = a cloned index");
        let root = env_path(CACHE_DIR_ENV).expect("FASTEMBED_CACHE_DIR = a model dir");
        let conn = crate::db::open_readonly(Path::new(&db)).unwrap();
        // A fixed pseudo-random spread of embedded prose, in 128-message batches as
        // run_embed_job claims them.
        let rows: Vec<(i64, String)> = conn
            .prepare(&format!(
                "SELECT m.id, m.text FROM messages m WHERE m.embedded = 1 AND {EMBEDDABLE_SQL}
                 ORDER BY (m.id * 2654435761) % 4294967296 LIMIT 3000"
            ))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let batches = |want: usize| -> Vec<Vec<String>> {
            let mut out = Vec::new();
            let mut n = 0;
            for b in rows.chunks(128) {
                let mut texts = chunk_messages(b).1;
                texts.truncate(want - n);
                n += texts.len();
                out.push(texts);
                if n >= want {
                    break;
                }
            }
            out
        };
        let mut reference: Option<Vec<Vec<f32>>> = None;
        for want in [100, 2000] {
            let work = batches(want);
            for threads in [1, 2, 4, 8] {
                let e = Embedder::new(false, Some(root.clone()), threads);
                let load = crate::jobclock::JobClock::start();
                e.ensure_loaded().unwrap();
                let load_cpu = load.process_cpu().unwrap_or_default();
                let clock = crate::jobclock::JobClock::start();
                let mut vecs = Vec::new();
                for texts in &work {
                    vecs.extend(e.embed(texts.clone()).unwrap());
                }
                let (wall, cpu) = (clock.wall(), clock.process_cpu().unwrap_or_default());
                let n = vecs.len() as f64;
                println!(
                    "{want:>5} chunks, {threads} threads: {:.0} chunks/s, {:.1} chunks per cpu-s \
                     ({:.2} s wall, {:.2} s cpu; load {:.2} s cpu)",
                    n / wall.as_secs_f64(),
                    n / cpu.as_secs_f64().max(1e-6),
                    wall.as_secs_f64(),
                    cpu.as_secs_f64(),
                    load_cpu.as_secs_f64(),
                );
                // Thread count must not move the vectors.
                if want == 100 {
                    match &reference {
                        None => reference = Some(vecs),
                        Some(r) => {
                            for (a, b) in r.iter().zip(&vecs) {
                                assert!(cosine(a, b) > 0.99999, "cosine {}", cosine(a, b));
                            }
                        }
                    }
                }
            }
        }
    }

    // ---- repeated chunk texts -------------------------------------------------------

    /// A text repeated within a batch reaches the model once, and one the build already
    /// stored never does; the vectors still come back parallel to the input.
    #[test]
    fn repeated_texts_run_through_the_model_once() {
        let texts: Vec<String> = ["a", "b", "a", "c", "b", "a"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        let keys: Vec<ChunkKey> = texts.iter().map(|t| ChunkKey::of(t)).collect();
        let mut known = std::collections::HashMap::new();
        known.insert(ChunkKey::of("c"), vec![3.0_f32]);
        let mut seen: Vec<String> = Vec::new();
        let (vecs, reused) = embed_unique_with(&texts, &keys, &known, |todo| {
            seen = todo.to_vec();
            Ok(todo
                .iter()
                .map(|t| vec![if t == "a" { 1.0 } else { 2.0 }])
                .collect())
        })
        .unwrap();
        assert_eq!(seen, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(reused, 4);
        let flat: Vec<f32> = vecs.iter().map(|v| v[0]).collect();
        assert_eq!(flat, vec![1.0, 2.0, 1.0, 3.0, 2.0, 1.0]);
        // Everything known: no model call at all (an embedder without a model succeeds).
        let all: std::collections::HashMap<ChunkKey, Vec<f32>> =
            keys.iter().map(|k| (*k, vec![0.5])).collect();
        let e = Embedder::new(false, Some(temp_dir("memo-nomodel")), 1);
        let rows: Vec<(i64, String)> = vec![(1, "a".into()), (2, "b".into())];
        let batch = embed_rows_with(&e, &rows, &all).unwrap();
        assert_eq!(batch.owners, vec![1, 2]);
        assert_eq!(batch.reused, 2);
        assert!(!e.is_loaded());
        // Same text, different length (or any other text): a different key.
        assert_ne!(ChunkKey::of("a"), ChunkKey::of("a "));
    }

    /// The memo hands back the vector the build stored for a text, and forgets one whose
    /// message (and so its vector) went away.
    #[test]
    fn chunk_memo_reuses_stored_vectors_until_they_go() {
        let mut db = temp_db("memo");
        // A subagent prompt a workflow sent to many subagents.
        let prompt = "Review the billing module for untested error paths";
        let first = insert_msg(&db.conn, 0, "user", prompt, None);
        let other = insert_msg(&db.conn, 1, "user", "something else", None);
        let rows = pending_batch(&db.conn, 10).unwrap();
        let (owners, texts) = chunk_messages(&rows);
        let keys: Vec<ChunkKey> = texts.iter().map(|t| ChunkKey::of(t)).collect();
        let vecs: Vec<Vec<f32>> = owners.iter().map(|o| direction(*o as f32 / 10.0)).collect();
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let rowids = store_batch(&mut db.conn, &ids, &owners, &vecs).unwrap();
        assert!(rowids.iter().all(Option::is_some));
        let mut memo = ChunkMemo::default();
        memo.remember(&keys, &rowids);
        assert_eq!(memo.len(), 2);

        // A later message repeating the first text.
        let again = insert_msg(&db.conn, 2, "user", prompt, None);
        let rows = pending_batch(&db.conn, 10).unwrap();
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![again]);
        let hits = memo.reusable(&rows);
        assert_eq!(hits.len(), 1);
        let known = memo.read_vectors(&db.conn, &hits);
        let want = &vecs[owners.iter().position(|o| *o == first).unwrap()];
        assert_eq!(known.get(&hits[0].0), Some(want));

        // Its vector goes with its message; the memo drops the entry instead of copying it.
        db.conn
            .execute("DELETE FROM messages WHERE id = ?1", [first])
            .unwrap();
        assert!(memo.read_vectors(&db.conn, &hits).is_empty());
        assert!(memo.reusable(&rows).is_empty());
        assert_eq!(memo.len(), 1);
        let _ = other;

        // Unstored vectors (a message gone mid-batch) are not remembered.
        let mut memo = ChunkMemo::default();
        memo.remember(&[ChunkKey::of("x")], &[None]);
        assert!(memo.is_empty());
    }

    /// A query issued while a big bulk batch runs must wait at most about one sub-batch,
    /// not the whole batch. Needs the real model (downloads on first run):
    /// `cargo test -- --ignored query_not_starved_by_bulk --nocapture`
    #[test]
    #[ignore]
    fn query_not_starved_by_bulk() {
        let e = std::sync::Arc::new(Embedder::default());
        e.ensure_loaded().unwrap();
        // ~1,024 chunks of ~1,600 chars: many seconds of inference in one `embed` call.
        let texts: Vec<String> = (0..1024)
            .map(|i| {
                format!(
                    "chunk {i}: {}",
                    "the quick brown fox jumps over the lazy dog ".repeat(36)
                )
            })
            .collect();
        let bulk = {
            let e = e.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                e.embed(texts).unwrap();
                start.elapsed()
            })
        };
        std::thread::sleep(Duration::from_millis(300)); // let the bulk job get going
        let start = Instant::now();
        let qv = embed_query(&e, "how do I activate a venv").unwrap();
        let query_wait = start.elapsed();
        let bulk_total = bulk.join().unwrap();
        assert_eq!(qv.map(|v| v.len()), Some(DIM));
        eprintln!("bulk took {bulk_total:?}; query answered in {query_wait:?}");
        assert!(
            query_wait < bulk_total / 4,
            "query waited {query_wait:?} behind a {bulk_total:?} bulk batch"
        );
    }

    /// Real model + vec0 path. Downloads bge-small into `model_cache_dir()` on first run
    /// (needs network): `cargo test -- --ignored embed_smoke --nocapture`
    #[test]
    #[ignore]
    fn embed_smoke() {
        let e = Embedder::default();
        let vecs = e
            .embed(vec!["how do I activate a python virtualenv".to_string()])
            .unwrap();
        assert_eq!(vecs[0].len(), DIM);
        assert!(
            model_is_cached(),
            "model should now be in {:?}",
            model_cache_dir()
        );
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
            None,
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
