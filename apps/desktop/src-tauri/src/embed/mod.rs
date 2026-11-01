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
use rusqlite::{params, Connection, ToSql};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

/// bge-small-en-v1.5 output dimensionality (matches vec_chunks `float[384]`).
pub const DIM: usize = 384;

/// Cosine-similarity floor for the semantic arm: KNN always returns its k-nearest, so a
/// query with no good match would otherwise inject noise. Anything below this is dropped.
/// Shared so the hybrid fusion can weight by similarity over the same [floor, 1.0] range.
pub(crate) const SEM_SIMILARITY_FLOOR: f32 = 0.35;

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
/// Mirrors `is_embeddable`, minus its blank-text check: that one is applied by
/// `chunk_messages` so the progress COUNTs never have to read message bodies.
pub const EMBEDDABLE_SQL: &str = "m.role IN ('user','assistant') AND m.tool_name IS NULL";

/// Rust-side twin of `EMBEDDABLE_SQL`: true for user/assistant prose that is worth a
/// vector. Blank text is excluded too (it would only produce a meaningless vector).
pub fn is_embeddable(role: &str, tool_name: Option<&str>, text: &str) -> bool {
    matches!(role, "user" | "assistant") && tool_name.is_none() && !text.trim().is_empty()
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

/// Lazily-loaded embedding model, held in Tauri managed state (and owned directly by the
/// `cal` / MCP sidecars).
///
/// Concurrency: fastembed's `embed` needs `&mut`, so the loaded model sits behind a mutex,
/// but that lock is only ever held for ONE short sub-batch of inference, and queries take
/// priority over the bulk build between sub-batches (`interactive`). Loading has its own
/// lock so a slow first download never blocks a caller that already has a model.
pub struct Embedder {
    /// Set exactly once, after a successful load.
    model: OnceLock<Mutex<TextEmbedding>>,
    /// Serializes loading; holds the last load failure (when, why) for the cooldown.
    init: Mutex<Option<(Instant, String)>>,
    /// Interactive callers (queries) waiting for or using the model. The bulk path yields
    /// between sub-batches while this is non-zero.
    interactive: AtomicUsize,
    /// False for sidecars (hooks, MCP, CLI): load only from the cache, never download.
    allow_download: bool,
    /// Test-only cache root override: the only root looked in and downloaded into. `None`
    /// means `lookup_roots()` / `model_cache_dir()`.
    cache_root: Option<PathBuf>,
}

impl Default for Embedder {
    /// The app's embedder: downloads the model on first use if it isn't cached yet.
    fn default() -> Self {
        Self::new(true, None)
    }
}

impl Embedder {
    fn new(allow_download: bool, cache_root: Option<PathBuf>) -> Self {
        Self {
            model: OnceLock::new(),
            init: Mutex::new(None),
            interactive: AtomicUsize::new(0),
            allow_download,
            cache_root,
        }
    }

    /// An embedder that NEVER downloads: for hooks, `cal` and the MCP server, which run
    /// inside an agent's prompt with the agent's CWD. Without a cached model,
    /// `embed_query` returns `Ok(None)` (callers fall back to keyword search),
    /// `embed_pending_facts*` leave facts pending for the app, and `embed` /
    /// `ensure_loaded` return `EmbedError::ModelNotCached`.
    pub fn cached_only() -> Self {
        Self::new(false, None)
    }

    /// True once the model is loaded in this process.
    pub fn is_loaded(&self) -> bool {
        self.model.get().is_some()
    }

    /// Load the model now (downloading it if this embedder allows that and it isn't
    /// cached), retrying even if a recent attempt failed. Call it before a bulk job so a
    /// load failure aborts the job instead of being mistaken for N bad batches, and from a
    /// background thread to prefetch the model.
    pub fn ensure_loaded(&self) -> Result<(), EmbedError> {
        self.load(LoadMode::Explicit).map(|_| ())
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
        self.infer_bulk(model, texts)
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

    /// Return the loaded model, loading it first if needed (subject to `mode` and
    /// `allow_download`).
    fn load(&self, mode: LoadMode) -> Result<&Mutex<TextEmbedding>, EmbedError> {
        if let Some(model) = self.model.get() {
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
        if let Some(model) = self.model.get() {
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
        let loaded = match find_snapshot(&roots) {
            Some(snapshot) => load_snapshot(&snapshot),
            None if may_download => download_model(&self.download_root()),
            // Not a failure (nothing was attempted), so it doesn't start the cooldown.
            None => return Err(EmbedError::ModelNotCached(roots)),
        };
        match loaded {
            Ok(model) => {
                *last_failure = None;
                Ok(self.model.get_or_init(|| Mutex::new(model)))
            }
            Err(e) => {
                let why = format!("{e:#}");
                *last_failure = Some((Instant::now(), why.clone()));
                Err(EmbedError::ModelInit(why))
            }
        }
    }

    /// Bulk inference: one short locked call per sub-batch, deferring to queries between.
    fn infer_bulk(
        &self,
        model: &Mutex<TextEmbedding>,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        for sub in texts.chunks(BULK_SUB_BATCH) {
            self.yield_to_interactive();
            let vecs = lock_model(model)
                .embed(sub, None)
                .map_err(inference_error)?;
            out.extend(vecs);
        }
        debug_assert!(
            out.iter().all(|v| v.len() == DIM),
            "unexpected embedding dim"
        );
        Ok(out)
    }

    /// Interactive inference (queries): registers as waiting so the bulk path steps aside
    /// after its current sub-batch, then runs in a single call.
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

/// ONNX intra-op threads: capped so a build doesn't pin every core and starve the UI
/// thread. fastembed counts logical cores; leave 2 free.
fn intra_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(1)
}

/// Load bge-small from a complete snapshot directory (see `cached_snapshot`), reading the
/// files directly. Going through fastembed's hf-hub loader instead would re-resolve the
/// cache from `$HF_HOME`, which may point somewhere else entirely. Same session options,
/// tokenizer, pooling and quantization as `TextEmbedding::try_new`, so the vectors match.
fn load_snapshot(snapshot: &Path) -> Result<TextEmbedding> {
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
        InitOptionsUserDefined::new().with_intra_threads(intra_threads()),
    )
    .with_context(context)
}

/// Download bge-small into `cache_root` (an absolute dir) through fastembed's hf-hub
/// loader, then load it. Only the app does this; callers decide beforehand whether a
/// download is allowed.
fn download_model(cache_root: &Path) -> Result<TextEmbedding> {
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
            .with_intra_threads(intra_threads()),
    )
    .with_context(|| format!("loading bge-small-en-v1.5 from {}", cache_root.display()))
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

/// Select up to `batch` not-yet-embedded messages that pass `EMBEDDABLE_SQL`. The
/// `embedded` flag is maintained by a partial index, so this is O(batch), not a growing
/// scan.
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
/// Blank messages produce no chunk; `store_batch` still marks them done.
pub fn chunk_messages(rows: &[(i64, String)]) -> (Vec<i64>, Vec<String>) {
    let mut owners = Vec::new();
    let mut texts = Vec::new();
    for (id, text) in rows {
        if text.trim().is_empty() {
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
    /// Messages whose inference failed even when retried on their own. They have no
    /// vectors; `store_batch` still marks them embedded so the job moves past them (FTS
    /// still finds them).
    pub failed: Vec<i64>,
}

/// Chunk and embed a claimed batch (the unlocked middle phase of the job), isolating bad
/// inputs: if the batch fails inference, each message is retried alone and only the ones
/// that still fail land in `failed`.
///
/// Returns `Err` only when the MODEL is unavailable (`EmbedError::is_model_unavailable`).
/// The caller must then stop the job and leave the rows pending (do NOT mark them
/// embedded), so they are embedded once the model loads.
pub fn embed_rows(
    embedder: &Embedder,
    rows: &[(i64, String)],
) -> Result<EmbeddedBatch, EmbedError> {
    let (owners, texts) = chunk_messages(rows);
    match embedder.embed_typed(&texts) {
        Ok(vecs) => {
            return Ok(EmbeddedBatch {
                owners,
                vecs,
                failed: Vec::new(),
            })
        }
        Err(e) if e.is_model_unavailable() => return Err(e),
        Err(_) => {} // one input broke the batch: isolate it below
    }
    let mut batch = EmbeddedBatch::default();
    for row in rows {
        let (owners, texts) = chunk_messages(std::slice::from_ref(row));
        match embedder.embed_typed(&texts) {
            Ok(vecs) => {
                batch.owners.extend(owners);
                batch.vecs.extend(vecs);
            }
            Err(e) if e.is_model_unavailable() => return Err(e),
            Err(_) => batch.failed.push(row.0),
        }
    }
    Ok(batch)
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
pub fn store_batch(
    conn: &mut Connection,
    message_ids: &[i64],
    owners: &[i64],
    vecs: &[Vec<f32>],
) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    {
        let mut live = tx.prepare("SELECT 1 FROM messages WHERE id = ?1 AND embedded = 0")?;
        let mut ins =
            tx.prepare("INSERT INTO vec_chunks (message_id, embedding) VALUES (?1, ?2)")?;
        let mut own = tx.prepare(
            "INSERT OR REPLACE INTO vec_chunk_owner (chunk_rowid, message_id) VALUES (?1, ?2)",
        )?;
        let mut alive: std::collections::HashMap<i64, bool> = std::collections::HashMap::new();
        for (mid, v) in owners.iter().zip(vecs.iter()) {
            let ok = match alive.get(mid) {
                Some(b) => *b,
                None => {
                    let b = live.exists([mid])?;
                    alive.insert(*mid, b);
                    b
                }
            };
            if !ok {
                continue; // deleted / re-upserted mid-batch: no orphan vectors
            }
            ins.execute(params![mid, vec_to_bytes(v)])?;
            own.execute(params![tx.last_insert_rowid(), mid])?;
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

/// Mark messages embedded WITHOUT storing vectors, so the job advances past inputs that
/// failed INFERENCE instead of retrying them forever. They stay FTS-searchable.
///
/// Never call this for a model-load failure (`is_model_unavailable`): that would flag the
/// whole corpus embedded with no vectors. Stop the job and leave the batch pending.
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
    let qv = embedder.infer_interactive(model, &[format!("{QUERY_PREFIX}{query}")])?;
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
    let project = project.filter(|p| !p.is_empty());
    // Over-fetch chunks so dedup + filtering still leaves k messages. vec0 applies the
    // source/subagent/project filter AFTER the KNN, so a selective filter can starve
    // results — over-fetch far more candidates when one is active. sqlite-vec requires `k`
    // as a literal (not a bound parameter), so inline our own integer.
    let selective = !sources.is_empty() || project.is_some();
    let knn_k = if selective {
        (k * 20).max(800)
    } else {
        (k * 5).max(200)
    };

    // MATERIALIZED so SQLite doesn't inline the KNN into the outer query (vec0 only
    // permits `ORDER BY distance` on the KNN query itself, not the outer ORDER BY).
    // EMBEDDABLE_SQL drops vectors that older builds stored for tool-call rows, so they
    // can't crowd prose out of the results before a migration deletes them.
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
         WHERE {EMBEDDABLE_SQL}"
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
    // Scope the semantic arm to the project too (was previously ignored, leaking cross-
    // project hits into a project-scoped hybrid search). Match how facts/threads aggregate.
    if let Some(p) = project {
        args.push(Box::new(format!("%{p}%")));
        sql.push_str(&format!(
            " AND COALESCE(t.project_key, t.project_path) LIKE ?{}",
            args.len()
        ));
    }
    // Similarity floor: KNN always returns its k-nearest, so a query with no good semantic
    // match would otherwise inject irrelevant chunks that RRF then promotes. Drop anything
    // below SEM_SIMILARITY_FLOOR (distance = 1 - cosine). Conservative so real matches survive.
    args.push(Box::new(1.0_f64 - SEM_SIMILARITY_FLOOR as f64));
    let dist_ph = args.len();
    args.push(Box::new(k as i64));
    let limit_ph = args.len();
    sql.push_str(&format!(
        " GROUP BY knn.message_id HAVING d <= ?{dist_ph} ORDER BY d LIMIT ?{limit_ph}"
    ));

    let arg_refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(arg_refs.as_slice(), |r| {
        let id: i64 = r.get(0)?;
        let dist: f64 = r.get(1)?;
        Ok((id, (1.0 - dist) as f32)) // cosine distance -> similarity
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Count of embedded messages vs total embeddable (for UI progress). Both counts use
/// `EMBEDDABLE_SQL`, the same predicate as `pending_batch`, so the bar reaches 100% when
/// the queue is empty instead of counting tool-call rows that are never embedded.
pub fn embed_progress(conn: &Connection) -> Result<(i64, i64)> {
    let done: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages m WHERE m.embedded = 1 AND {EMBEDDABLE_SQL}"),
        [],
        |r| r.get(0),
    )?;
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages m WHERE {EMBEDDABLE_SQL}"),
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
        let mut direct = load_snapshot(&snapshot).unwrap();
        let mut hub =
            TextEmbedding::try_new(TextInitOptions::new(MODEL).with_cache_dir(root)).unwrap();
        assert_eq!(
            direct.embed(texts, None).unwrap(),
            hub.embed(texts, None).unwrap()
        );
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
        let e = Embedder::new(false, Some(root.clone()));

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
        let e = Embedder::new(true, Some(temp_dir("blank")));
        assert!(embed_query(&e, "   ").unwrap().is_none());
        assert!(!e.is_loaded());
    }

    #[test]
    fn corrupt_cache_is_model_init_error_with_cooldown() {
        // A cached model is read from its snapshot directly, so HF_HOME can't redirect this.
        let root = temp_dir("corrupt");
        fake_model_cache(&root, None);
        let e = Embedder::new(false, Some(root.clone()));

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
        mark_embedded(conn, &[lost, tool]).unwrap();

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
