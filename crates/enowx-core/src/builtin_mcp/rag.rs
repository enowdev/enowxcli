//! Retrieval over the project's own code: chunks embedded by Voyage AI,
//! OpenAI or any OpenAI-compatible endpoint (Ollama, LM Studio, Jina, ...)
//! and kept in Postgres with pgvector, local or in the cloud.
//!
//! Indexing walks the workspace the way git sees it (`.gitignore` honoured,
//! `.env` files never read), cuts each file into chunks of whole lines that
//! remember their line range, and embeds only the chunks whose content
//! changed since the last run. Chunks of files that were removed, or of the
//! tail of a file that got shorter, are deleted. Search combines the vector
//! match with a lexical one (reciprocal rank fusion) and reranks the
//! candidates with a reranker when one is set (Voyage's by default).
//!
//! Each chunk records the model that embedded it. Switching the model makes
//! the next `index` embed the project again rather than mix vectors from two
//! models, and a search only compares vectors of the model in use.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{schema, Server, ToolSpec};

mod chunk;

/// The default embedding model: Voyage's model for code, at its default width.
pub const MODEL: &str = "voyage-code-3";
pub const DIM: usize = 1024;
/// The default reranker, for Voyage.
pub const RERANK_MODEL: &str = "rerank-2.5";

const VOYAGE: &str = "https://api.voyageai.com/v1";
const OPENAI: &str = "https://api.openai.com/v1";
/// pgvector's HNSW index takes vectors up to this width; wider ones are
/// searched exactly, without the index.
const HNSW_MAX: usize = 2000;
/// Files larger than this are not read.
const MAX_FILE: u64 = 512 * 1024;
/// Chunks per embedding request.
const BATCH: usize = 64;
/// Candidates fetched before reranking.
const RECALL: i64 = 40;

/// Where the embeddings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Voyage,
    OpenAi,
    /// Any endpoint that speaks OpenAI's `/embeddings`: Ollama, LM Studio,
    /// Jina, Mistral, Together, a gateway.
    Custom,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Voyage, Provider::OpenAi, Provider::Custom];

    pub fn id(self) -> &'static str {
        match self {
            Provider::Voyage => "voyage",
            Provider::OpenAi => "openai",
            Provider::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Voyage => "Voyage AI",
            Provider::OpenAi => "OpenAI",
            Provider::Custom => "Custom (OpenAI-compatible)",
        }
    }

    pub fn parse(id: &str) -> Provider {
        match id.trim().to_ascii_lowercase().as_str() {
            "openai" => Provider::OpenAi,
            "custom" | "openai-compatible" | "compatible" => Provider::Custom,
            _ => Provider::Voyage,
        }
    }

    /// The models offered for picking, each with the widths it can return
    /// (its default first). Empty for a custom endpoint: its model is typed.
    pub fn models(self) -> &'static [(&'static str, &'static [usize])] {
        const VOYAGE_DIMS: &[usize] = &[1024, 256, 512, 2048];
        match self {
            Provider::Voyage => &[
                ("voyage-code-3", VOYAGE_DIMS),
                ("voyage-3.5", VOYAGE_DIMS),
                ("voyage-3.5-lite", VOYAGE_DIMS),
                ("voyage-3-large", VOYAGE_DIMS),
            ],
            Provider::OpenAi => &[
                ("text-embedding-3-small", &[1536, 512, 1024]),
                ("text-embedding-3-large", &[1024, 256, 1536, 3072]),
            ],
            Provider::Custom => &[],
        }
    }

    /// The rerankers offered for picking; a custom endpoint's is typed.
    pub fn rerankers(self) -> &'static [&'static str] {
        match self {
            Provider::Voyage => &["rerank-2.5", "rerank-2.5-lite"],
            Provider::OpenAi | Provider::Custom => &[],
        }
    }

    /// Where its API lives when no base URL is set.
    pub fn default_url(self) -> &'static str {
        match self {
            Provider::Voyage => VOYAGE,
            Provider::OpenAi => OPENAI,
            Provider::Custom => "",
        }
    }
}

/// The database used to store the RAG index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseBackend {
    Embedded,
    Postgres,
}

impl DatabaseBackend {
    /// Whether Oliphaunt's embedded PostgreSQL is built for this exact target.
    pub fn embedded_supported() -> bool {
        cfg!(any(
            all(
                target_arch = "x86_64",
                target_os = "linux",
                target_env = "gnu"
            ),
            all(
                target_arch = "aarch64",
                target_os = "linux",
                target_env = "gnu"
            ),
            all(
                target_arch = "aarch64",
                target_vendor = "apple",
                target_os = "macos"
            ),
            all(
                target_arch = "x86_64",
                target_os = "windows",
                target_env = "msvc"
            ),
        ))
    }
}

/// The non-secret half of the setup. The DSN and the API key are secrets and
/// live in `auth.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagSetup {
    /// Explicit database choice; absent in legacy setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseBackend>,
    /// `voyage` (the default), `openai` or `custom`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The API's base URL, for a custom endpoint (`http://localhost:11434/v1`
    /// for Ollama). Empty: the provider's own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// The embedding model; the provider's first when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// The vector width; the model's default when 0. Required for a custom
    /// endpoint, whose models enx does not know.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dimension: usize,
    /// The reranker: empty for the provider's default, `off` for none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rerank: String,
    /// Keep the index fresh by itself (the default): index when the server
    /// starts, then whatever changed, every half minute and before a search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_index: Option<bool>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl RagSetup {
    /// Resolve an explicit choice or the historical DSN-based default.
    pub fn effective_database_backend(&self, has_postgres_dsn: bool) -> Result<DatabaseBackend> {
        let backend = self.database.unwrap_or_else(|| {
            if has_postgres_dsn || !DatabaseBackend::embedded_supported() {
                DatabaseBackend::Postgres
            } else {
                DatabaseBackend::Embedded
            }
        });
        anyhow::ensure!(
            backend != DatabaseBackend::Embedded || DatabaseBackend::embedded_supported(),
            "embedded RAG is not supported on this target"
        );
        Ok(backend)
    }

    pub fn provider(&self) -> Provider {
        Provider::parse(&self.provider)
    }

    pub fn auto_index(&self) -> bool {
        self.auto_index.unwrap_or(true)
    }

    pub fn base_url(&self) -> String {
        let url = self.base_url.trim().trim_end_matches('/');
        if url.is_empty() {
            self.provider().default_url().to_owned()
        } else {
            url.to_owned()
        }
    }

    pub fn model(&self) -> String {
        let model = self.model.trim();
        if !model.is_empty() {
            return model.to_owned();
        }
        self.provider()
            .models()
            .first()
            .map(|(m, _)| (*m).to_owned())
            .unwrap_or_default()
    }

    pub fn dimension(&self) -> usize {
        if self.dimension > 0 {
            return self.dimension;
        }
        let model = self.model();
        self.provider()
            .models()
            .iter()
            .find(|(m, _)| *m == model)
            .and_then(|(_, dims)| dims.first().copied())
            .unwrap_or(0)
    }

    /// The reranker in use, if any.
    pub fn reranker(&self) -> Option<String> {
        let rerank = self.rerank.trim();
        if rerank.eq_ignore_ascii_case("off") {
            return None;
        }
        if !rerank.is_empty() {
            return Some(rerank.to_owned());
        }
        self.provider().rerankers().first().map(|r| (*r).to_owned())
    }

    /// What a chunk records as the model that embedded it.
    pub fn embedder(&self) -> String {
        format!(
            "{}:{}:{}",
            self.provider().id(),
            self.model(),
            self.dimension()
        )
    }

    /// Whether the setup can run: a model and a width, and a URL for a
    /// custom endpoint. The reason when it cannot.
    pub fn check(&self) -> Result<()> {
        if self.provider() == Provider::Custom {
            let url = self.base_url();
            anyhow::ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "a custom embedding endpoint needs a base URL (http:// or https://)"
            );
        }
        anyhow::ensure!(!self.model().is_empty(), "no embedding model set");
        anyhow::ensure!(
            self.dimension() > 0,
            "no vector width set for {}; give the dimension the model returns",
            self.model()
        );
        anyhow::ensure!(
            self.dimension() <= 16000,
            "pgvector stores at most 16000 dimensions"
        );
        Ok(())
    }
}

/// The table holding every project's chunks of one width: a change of
/// width never lands in a column of the wrong size.
fn table(dim: usize) -> String {
    format!("enx_rag_chunks_{dim}")
}

pub struct Rag {
    backend: DatabaseBackend,
    dsn: Option<String>,
    /// The embedding API's key; may be empty for a local endpoint.
    key: String,
    setup: RagSetup,
    model: String,
    dim: usize,
    url: String,
    http: reqwest::Client,
    db: Mutex<Option<Arc<tokio_postgres::Client>>>,
    /// The folder indexed when a call names none.
    workspace: PathBuf,
    /// One sync at a time: the background one and a tool's never race.
    syncing: Mutex<()>,
    /// Per folder, each file's stamp when it was last synced; what differs
    /// now is what changed.
    seen: Mutex<HashMap<PathBuf, BTreeMap<String, Stamp>>>,
    /// The last background sync's failure, for `status`.
    last_error: std::sync::Mutex<Option<String>>,
    /// Folders synced in full this run: until then a refresh is a full
    /// sync, which also drops what was deleted while enx was closed.
    full: Mutex<std::collections::HashSet<PathBuf>>,
    /// Files the session changed, relative to the workspace, to index at
    /// once (the hook after `write` and `edit`).
    queued: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Wakes the background loop for queued files or the end of a turn.
    wake: tokio::sync::Notify,
    /// Since when a turn has been running, as the client says. The full
    /// scan waits while one does; a claim older than 20 minutes is taken
    /// for a client that went away without saying so.
    busy_since: std::sync::Mutex<Option<std::time::Instant>>,
}

/// What tells a file changed without reading it: its size and mtime.
type Stamp = (u64, Option<std::time::SystemTime>);

/// What a sync did.
#[derive(Debug, Default)]
struct Synced {
    files: usize,
    chunks: usize,
    embedded: usize,
    moved: usize,
    removed: usize,
    replaced: u64,
}

/// A session-level PostgreSQL advisory lock. Dropping the driver closes the
/// dedicated session, which releases its lock on error or cancellation.
struct AdvisorySession {
    client: tokio_postgres::Client,
    driver: Option<tokio::task::JoinHandle<()>>,
}

impl AdvisorySession {
    async fn acquire(&self, key: i64) -> Result<()> {
        self.client
            .query_one("SELECT pg_advisory_lock($1)", &[&key])
            .await?;
        Ok(())
    }

    async fn release(&self, key: i64) -> Result<()> {
        let released: bool = self
            .client
            .query_one("SELECT pg_advisory_unlock($1)", &[&key])
            .await?
            .get(0);
        anyhow::ensure!(
            released,
            "PostgreSQL advisory lock was not held by this session"
        );
        Ok(())
    }
}

impl Drop for AdvisorySession {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

fn advisory_key(namespace: &str) -> i64 {
    // FNV-1a is stable across platforms and process runs (unlike DefaultHasher).
    stable_hash(namespace) as i64
}

impl Rag {
    pub fn new(
        backend: DatabaseBackend,
        dsn: Option<&str>,
        key: &str,
        setup: &RagSetup,
    ) -> Result<Self> {
        setup.check()?;
        if backend == DatabaseBackend::Postgres {
            let dsn = dsn.context("no database stored for rag; open Settings > RAG")?;
            anyhow::ensure!(
                dsn.starts_with("postgres://") || dsn.starts_with("postgresql://"),
                "the database must be a postgres:// connection string"
            );
        }
        // Canonical, as `root` makes a folder a tool names: the project id
        // hashes the path, and the background sync and a search must agree.
        let workspace = std::env::var_os("ENX_WORKSPACE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        Ok(Self {
            backend,
            dsn: if backend == DatabaseBackend::Postgres {
                dsn.map(|value| value.trim().to_owned())
            } else {
                None
            },
            key: key.trim().to_owned(),
            model: setup.model(),
            dim: setup.dimension(),
            url: setup.base_url(),
            setup: setup.clone(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            db: Mutex::new(None),
            workspace,
            syncing: Mutex::new(()),
            seen: Mutex::new(HashMap::new()),
            last_error: std::sync::Mutex::new(None),
            full: Mutex::new(std::collections::HashSet::new()),
            queued: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            wake: tokio::sync::Notify::new(),
            busy_since: std::sync::Mutex::new(None),
        })
    }

    /// A connection, opened once and reopened after it drops. Embedded
    /// PostgreSQL is started lazily and uses its local, trust-authenticated
    /// endpoint; external PostgreSQL retains the DSN's rustls behavior.
    async fn db(&self) -> Result<Arc<tokio_postgres::Client>> {
        let mut slot = self.db.lock().await;
        if let Some(client) = slot.as_ref() {
            if !client.is_closed() {
                return Ok(client.clone());
            }
        }
        let (client, driver, lease) = match self.backend {
            DatabaseBackend::Embedded => {
                let (endpoint, lease) = super::rag_db::ensure_server().await?;
                let config: tokio_postgres::Config =
                    endpoint.parse().context("the embedded database endpoint")?;
                let (client, connection) = tokio::time::timeout(
                    Duration::from_secs(20),
                    config.connect(tokio_postgres::NoTls),
                )
                .await
                .context("no answer from the database within 20 seconds")?
                .context("connecting to embedded PostgreSQL")?;
                (
                    client,
                    tokio::spawn(async move {
                        let _ = connection.await;
                    }),
                    Some(lease),
                )
            }
            DatabaseBackend::Postgres => {
                let config: tokio_postgres::Config = self
                    .dsn
                    .as_deref()
                    .context("no PostgreSQL connection string")?
                    .parse()
                    .context("the database connection string")?;
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
                let connector = tokio_postgres_rustls::MakeRustlsConnect::new(tls);
                let (client, connection) =
                    tokio::time::timeout(Duration::from_secs(20), config.connect(connector))
                        .await
                        .context("no answer from the database within 20 seconds")?
                        .context("connecting to the database")?;
                (
                    client,
                    tokio::spawn(async move {
                        let _ = connection.await;
                    }),
                    None,
                )
            }
        };
        // The client and its driver are live, so readiness is established;
        // release the startup lease before opening another local session.
        drop(lease);
        let client = Arc::new(client);
        let prepared = async {
            let schema_lock = self.lock_session().await?;
            let key = advisory_key("enx-rag-schema-v1");
            schema_lock.acquire(key).await?;
            prepare(&client, self.dim).await?;
            schema_lock.release(key).await?;
            drop(schema_lock);
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = prepared {
            driver.abort();
            return Err(error);
        }
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Opens an independent session, used for session-level advisory locks.
    async fn lock_session(&self) -> Result<AdvisorySession> {
        let (client, driver) = match self.backend {
            DatabaseBackend::Embedded => {
                let (endpoint, _lease) = super::rag_db::ensure_server().await?;
                let config: tokio_postgres::Config =
                    endpoint.parse().context("the embedded database endpoint")?;
                let (client, connection) = tokio::time::timeout(
                    Duration::from_secs(20),
                    config.connect(tokio_postgres::NoTls),
                )
                .await
                .context("no answer from the database within 20 seconds")?
                .context("connecting to embedded PostgreSQL")?;
                (
                    client,
                    tokio::spawn(async move {
                        let _ = connection.await;
                    }),
                )
            }
            DatabaseBackend::Postgres => {
                let config: tokio_postgres::Config = self
                    .dsn
                    .as_deref()
                    .context("no PostgreSQL connection string")?
                    .parse()
                    .context("the database connection string")?;
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
                let connector = tokio_postgres_rustls::MakeRustlsConnect::new(tls);
                let (client, connection) =
                    tokio::time::timeout(Duration::from_secs(20), config.connect(connector))
                        .await
                        .context("no answer from the database within 20 seconds")?
                        .context("connecting to the database")?;
                (
                    client,
                    tokio::spawn(async move {
                        let _ = connection.await;
                    }),
                )
            }
        };
        Ok(AdvisorySession {
            client,
            driver: Some(driver),
        })
    }

    /// Embed `inputs` as documents or as a query.
    async fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        #[derive(Deserialize)]
        struct Item {
            embedding: Vec<f32>,
            #[serde(default)]
            index: Option<usize>,
        }
        #[derive(Deserialize)]
        struct Reply {
            data: Vec<Item>,
        }
        let provider = self.setup.provider();
        let mut body = json!({ "input": inputs, "model": self.model });
        match provider {
            Provider::Voyage => {
                body["input_type"] = json!(if query { "query" } else { "document" });
                body["output_dimension"] = json!(self.dim);
            }
            // Only the v3 models take a width; older ones return their own.
            Provider::OpenAi if self.model.starts_with("text-embedding-3") => {
                body["dimensions"] = json!(self.dim);
            }
            // An unknown endpoint gets the plain request: some refuse fields
            // they do not know. Its width is checked on the way back.
            _ => {}
        }
        let mut request = self
            .http
            .post(format!("{}/embeddings", self.url))
            .json(&body);
        if !self.key.is_empty() {
            request = request.bearer_auth(&self.key);
        }
        let who = provider.label();
        let response = request
            .send()
            .await
            .with_context(|| format!("reaching {who} at {}", self.url))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                bail!("{who} refused the API key ({status}); set it again in Settings > RAG");
            }
            bail!(
                "{who} answered {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let reply: Reply =
            serde_json::from_str(&text).with_context(|| format!("reading {who}'s reply"))?;
        let mut vectors = vec![Vec::new(); inputs.len()];
        for (position, item) in reply.data.into_iter().enumerate() {
            if let Some(slot) = vectors.get_mut(item.index.unwrap_or(position)) {
                *slot = item.embedding;
            }
        }
        if let Some(wrong) = vectors.iter().find(|v| v.len() != self.dim) {
            bail!(
                "{} returned vectors {} wide, not the {} set; set the dimension to {} in Settings > RAG",
                self.model,
                wrong.len(),
                self.dim,
                wrong.len()
            );
        }
        Ok(vectors)
    }

    /// The candidates in the reranker's order, or `None` when it fails (the
    /// fused order is used then).
    async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top: usize,
    ) -> Option<Vec<(usize, f32)>> {
        #[derive(Deserialize)]
        struct Item {
            index: usize,
            relevance_score: f32,
        }
        // Voyage answers `data`; Cohere, Jina and most others `results`.
        #[derive(Deserialize)]
        struct Reply {
            #[serde(default)]
            data: Vec<Item>,
            #[serde(default)]
            results: Vec<Item>,
        }
        let model = self.setup.reranker()?;
        let mut body = json!({ "query": query, "documents": documents, "model": model });
        // Voyage takes `top_k`; others name it differently or refuse it, so
        // they get none and the answer is cut here.
        if self.setup.provider() == Provider::Voyage {
            body["top_k"] = json!(top);
        }
        let mut request = self.http.post(format!("{}/rerank", self.url)).json(&body);
        if !self.key.is_empty() {
            request = request.bearer_auth(&self.key);
        }
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let reply: Reply = response.json().await.ok()?;
        let mut items = if reply.data.is_empty() {
            reply.results
        } else {
            reply.data
        };
        items.sort_by(|a, b| b.relevance_score.total_cmp(&a.relevance_score));
        items.truncate(top);
        (!items.is_empty()).then(|| {
            items
                .into_iter()
                .map(|i| (i.index, i.relevance_score))
                .collect()
        })
    }

    fn root(&self, args: &Value) -> Result<PathBuf> {
        let root = match args
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
        {
            Some(path) => PathBuf::from(path),
            None => self.workspace.clone(),
        };
        let root = std::fs::canonicalize(&root)
            .with_context(|| format!("no folder at {}", root.display()))?;
        anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
        Ok(root)
    }

    /// Bring the index of `root` up to date, for every file or for `only`
    /// (paths relative to `root`, removed ones included). New and changed
    /// chunks are embedded; chunks that only moved get their new lines
    /// without being embedded again; chunks no file produces any more go.
    async fn sync(&self, root: &Path, only: Option<Vec<String>>) -> Result<Synced> {
        let _one = self.syncing.lock().await;
        let project = project_id(root);
        let project_lock = self.lock_session().await?;
        let project_key = advisory_key(&format!("enx-rag-project-v1:{project}"));
        project_lock.acquire(project_key).await?;
        let embedder = self.setup.embedder();
        let db = self.db().await?;
        let t = table(self.dim);
        // Chunks a different model embedded cannot be compared with this
        // one's: they go, and the project is embedded again.
        let replaced = db
            .execute(
                &format!("DELETE FROM {t} WHERE project_id = $1 AND embed_model <> $2"),
                &[&project, &embedder],
            )
            .await?;
        let mut report = Synced {
            replaced,
            ..Default::default()
        };
        let listing = tokio::task::spawn_blocking({
            let root = root.to_path_buf();
            move || {
                lower_priority();
                files(&root)
            }
        })
        .await?;
        let targets: Vec<String> = match &only {
            Some(only) => only.clone(),
            None => listing.keys().cloned().collect(),
        };
        let chunks = tokio::task::spawn_blocking({
            let root = root.to_path_buf();
            let present: Vec<String> = targets
                .iter()
                .filter(|rel| listing.contains_key(*rel))
                .cloned()
                .collect();
            move || {
                lower_priority();
                chunks_of(&root, &present)
            }
        })
        .await?;
        report.files = chunks
            .iter()
            .map(|c| c.file.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        report.chunks = chunks.len();

        let rows = match &only {
            None => {
                db.query(
                    &format!("SELECT id, content_hash, start_line, end_line FROM {t} WHERE project_id = $1"),
                    &[&project],
                )
                .await?
            }
            Some(_) => {
                db.query(
                    &format!(
                        "SELECT id, content_hash, start_line, end_line FROM {t} WHERE project_id = $1 AND source_file = ANY($2)"
                    ),
                    &[&project, &targets],
                )
                .await?
            }
        };
        let existing: HashMap<String, (String, i32, i32)> = rows
            .into_iter()
            .map(|row| -> Result<_> {
                Ok((
                    row.try_get(0)?,
                    (row.try_get(1)?, row.try_get(2)?, row.try_get(3)?),
                ))
            })
            .collect::<Result<_>>()?;

        let mut changed: Vec<&Chunk> = Vec::new();
        for chunk in &chunks {
            match existing.get(&chunk.id) {
                Some((hash, start, end)) if *hash == chunk.hash => {
                    if *start != chunk.start as i32 || *end != chunk.end as i32 {
                        db.execute(
                            &format!("UPDATE {t} SET start_line = $3, end_line = $4 WHERE project_id = $1 AND id = $2"),
                            &[&project, &chunk.id, &(chunk.start as i32), &(chunk.end as i32)],
                        )
                        .await?;
                        report.moved += 1;
                    }
                }
                _ => changed.push(chunk),
            }
        }
        for batch in changed.chunks(BATCH) {
            let inputs: Vec<String> = batch.iter().map(|c| c.text()).collect();
            let vectors = self.embed(&inputs, false).await?;
            for (chunk, vector) in batch.iter().zip(vectors) {
                db.execute(
                    &format!(
                        "INSERT INTO {t} (project_id, id, source_file, start_line, end_line, content, content_hash, embedding, embed_model, indexed_at)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::vector, $9, now())
                         ON CONFLICT (project_id, id) DO UPDATE SET
                           source_file = EXCLUDED.source_file, start_line = EXCLUDED.start_line,
                           end_line = EXCLUDED.end_line, content = EXCLUDED.content,
                           content_hash = EXCLUDED.content_hash, embedding = EXCLUDED.embedding,
                           embed_model = EXCLUDED.embed_model, indexed_at = now()"
                    ),
                    &[
                        &project,
                        &chunk.id,
                        &chunk.file,
                        &(chunk.start as i32),
                        &(chunk.end as i32),
                        &chunk.text(),
                        &chunk.hash,
                        &vector_literal(&vector),
                        &embedder,
                    ],
                )
                .await?;
                report.embedded += 1;
            }
        }
        let wanted: std::collections::HashSet<&str> =
            chunks.iter().map(|c| c.id.as_str()).collect();
        let stale: Vec<String> = existing
            .keys()
            .filter(|id| !wanted.contains(id.as_str()))
            .cloned()
            .collect();
        if !stale.is_empty() {
            report.removed = db
                .execute(
                    &format!("DELETE FROM {t} WHERE project_id = $1 AND id = ANY($2)"),
                    &[&project, &stale],
                )
                .await? as usize;
        }

        if only.is_none() {
            self.full.lock().await.insert(root.to_path_buf());
        }
        // What was synced is what the next refresh compares against.
        let mut seen = self.seen.lock().await;
        let stamps = seen.entry(root.to_path_buf()).or_default();
        match &only {
            None => {
                *stamps = listing
                    .iter()
                    .map(|(rel, (_, stamp))| (rel.clone(), *stamp))
                    .collect()
            }
            Some(only) => {
                for rel in only {
                    match listing.get(rel) {
                        Some((_, stamp)) => {
                            stamps.insert(rel.clone(), *stamp);
                        }
                        None => {
                            stamps.remove(rel);
                        }
                    }
                }
            }
        }
        project_lock.release(project_key).await?;
        drop(project_lock);
        Ok(report)
    }
    /// Sync what changed since the last sync of `root`: every file the
    /// first time, then only files whose size or mtime moved, or that
    /// appeared or went. Cheap when nothing changed: a walk and a stat.
    async fn refresh(&self, root: &Path) -> Result<Option<Synced>> {
        if !self.full.lock().await.contains(root) {
            return self.sync(root, None).await.map(Some);
        }
        let previous = self
            .seen
            .lock()
            .await
            .get(root)
            .cloned()
            .unwrap_or_default();
        let listing = tokio::task::spawn_blocking({
            let root = root.to_path_buf();
            move || {
                lower_priority();
                files(&root)
            }
        })
        .await?;
        let mut changed: Vec<String> = listing
            .iter()
            .filter(|(rel, (_, stamp))| previous.get(*rel) != Some(stamp))
            .map(|(rel, _)| rel.clone())
            .collect();
        changed.extend(
            previous
                .keys()
                .filter(|rel| !listing.contains_key(*rel))
                .cloned(),
        );
        if changed.is_empty() {
            return Ok(None);
        }
        self.sync(root, Some(changed)).await.map(Some)
    }

    /// Whether a turn is running, by the client's last word.
    fn busy(&self) -> bool {
        self.busy_since
            .lock()
            .ok()
            .and_then(|since| *since)
            .is_some_and(|since| since.elapsed() < Duration::from_secs(20 * 60))
    }

    /// A path the agent wrote, relative to the workspace with `/`, or None
    /// when it lies outside. A deleted file is placed by its folder.
    fn relative(&self, path: &str) -> Option<String> {
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        let canonical = std::fs::canonicalize(&absolute).ok().or_else(|| {
            let folder = std::fs::canonicalize(absolute.parent()?).ok()?;
            Some(folder.join(absolute.file_name()?))
        })?;
        let relative = canonical
            .strip_prefix(&self.workspace)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        (!relative.is_empty()).then_some(relative)
    }

    async fn index(&self, args: &Value) -> Result<String> {
        let root = self.root(args)?;
        let project = project_id(&root);
        let report = self.sync(&root, None).await?;
        let unchanged = report.chunks - report.embedded;
        let mut summary = format!(
            "Indexed {} ({project}) with {}: {} files, {} chunks; embedded {} new or changed, {} moved, removed {} stale, {unchanged} unchanged.",
            root.display(),
            self.model,
            report.files,
            report.chunks,
            report.embedded,
            report.moved,
            report.removed,
        );
        if report.replaced > 0 {
            summary.push_str(&format!(
                " The {} chunks a previous model embedded were replaced.",
                report.replaced
            ));
        }
        Ok(summary)
    }

    async fn search(&self, args: &Value) -> Result<String> {
        let query = super::arg(args, "query")?.to_owned();
        let limit = args
            .get("limit")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(8)
            .clamp(1, 30) as usize;
        let root = self.root(args)?;
        // Fresh before it answers: whatever changed since the last sync is
        // indexed first. A sync already running (the first full one, on a
        // large project) is not waited for.
        let mut note = String::new();
        if self.setup.auto_index() {
            if self.syncing.try_lock().is_err() {
                note = "\n\n(Indexing is still running; results may miss the newest code.)".into();
            } else if let Err(error) = self.refresh(&root).await {
                note = format!("\n\n(Could not refresh the index first: {error:#})");
            }
        }
        let project = project_id(&root);
        let embedder = self.setup.embedder();
        let db = self.db().await?;
        let vector = self
            .embed(std::slice::from_ref(&query), true)
            .await?
            .pop()
            .context("no embedding for the query")?;
        let rows = db
            .query(
                &format!(
                    "WITH dense AS (
                       SELECT id, row_number() OVER (ORDER BY embedding <=> $1::text::vector) AS rank
                       FROM {t} WHERE project_id = $2 AND embed_model = $5
                       ORDER BY embedding <=> $1::text::vector LIMIT $3),
                     lexical AS (
                       SELECT id, row_number() OVER (ORDER BY ts_rank(content_tsv, plainto_tsquery('simple', $4)) DESC) AS rank
                       FROM {t} WHERE project_id = $2 AND embed_model = $5 AND content_tsv @@ plainto_tsquery('simple', $4)
                       LIMIT $3)
                     SELECT c.source_file, c.start_line, c.end_line, c.content,
                            (COALESCE(1.0 / (60 + d.rank), 0) + COALESCE(1.0 / (60 + l.rank), 0))::float8 AS score
                     FROM dense d FULL OUTER JOIN lexical l ON d.id = l.id
                     JOIN {t} c ON c.project_id = $2 AND c.id = COALESCE(d.id, l.id)
                     ORDER BY score DESC LIMIT $3",
                    t = table(self.dim)
                ),
                &[&vector_literal(&vector), &project, &RECALL, &query, &embedder],
            )
            .await?;
        if rows.is_empty() {
            let row = db
                .query_one(
                    &format!(
                        "SELECT count(*), count(*) FILTER (WHERE embed_model = $2) FROM {} WHERE project_id = $1",
                        table(self.dim)
                    ),
                    &[&project, &embedder],
                )
                .await?;
            let (indexed, current): (i64, i64) = (row.try_get(0)?, row.try_get(1)?);
            return Ok(if indexed > 0 && current == 0 {
                format!(
                    "{} was indexed with another model. Call `index` to embed it with {}.",
                    root.display(),
                    self.model
                )
            } else if indexed == 0 {
                format!("{} is not indexed yet. Call `index` first.", root.display())
            } else {
                format!("Nothing in {} matches that.", root.display())
            });
        }
        // Read with `try_get`: a column of an unexpected type is an error the
        // caller sees, not a panic that takes the server down.
        let hits: Vec<(String, i32, i32, String, f64)> = rows
            .into_iter()
            .map(|r| -> Result<_> {
                Ok((
                    r.try_get(0)?,
                    r.try_get(1)?,
                    r.try_get(2)?,
                    r.try_get(3)?,
                    r.try_get(4)?,
                ))
            })
            .collect::<Result<_>>()?;
        let documents: Vec<String> = hits.iter().map(|h| h.3.clone()).collect();
        let order: Vec<(usize, f32)> = match self.rerank(&query, &documents, limit).await {
            Some(order) => order,
            None => (0..hits.len().min(limit))
                .map(|i| (i, hits[i].4 as f32))
                .collect(),
        };
        let mut out = String::new();
        for (index, score) in order {
            let Some((file, start, end, content, _)) = hits.get(index) else {
                continue;
            };
            // The stored text leads with where the chunk is and what it
            // defines, for the embedding; the reader gets that as a heading.
            let (head, body) = content.split_once("\n\n").unwrap_or(("", content.as_str()));
            let mut about = String::new();
            for line in head.lines() {
                if let Some(scope) = line.strip_prefix("In: ") {
                    about.push_str(&format!(" · in {scope}"));
                } else if let Some(names) = line.strip_prefix("Defines: ") {
                    about.push_str(&format!(" · {names}"));
                }
            }
            out.push_str(&format!(
                "## {file}:{start}-{end}{about}  (score {score:.3})\n```\n{}\n```\n\n",
                body.trim_end()
            ));
        }
        Ok(format!("{}{note}", out.trim_end()))
    }

    async fn status(&self, args: &Value) -> Result<String> {
        let root = self.root(args)?;
        let project = project_id(&root);
        let db = self.db().await?;
        let row = db
            .query_one(
                &format!(
                    "SELECT count(*), count(DISTINCT source_file), max(indexed_at)::text FROM {} WHERE project_id = $1 AND embed_model = $2",
                    table(self.dim)
                ),
                &[&project, &self.setup.embedder()],
            )
            .await?;
        let (chunks, files, last): (i64, i64, Option<String>) =
            (row.try_get(0)?, row.try_get(1)?, row.try_get(2)?);
        Ok(render(&json!({
            "folder": root.display().to_string(),
            "project": project,
            "chunks": chunks,
            "files": files,
            "last_indexed": last,
            "provider": self.setup.provider().label(),
            "model": self.model,
            "dimension": self.dim,
            "reranker": self.setup.reranker().unwrap_or_else(|| "off".into()),
            "auto_index": self.setup.auto_index(),
            "indexing_now": self.syncing.try_lock().is_err(),
            "session_busy": self.busy(),
            "last_auto_index_error": self.last_error.lock().ok().and_then(|e| e.clone()),
        })))
    }

    async fn forget(&self, args: &Value) -> Result<String> {
        let _one = self.syncing.lock().await;
        let root = self.root(args)?;
        let project = project_id(&root);
        let project_lock = self.lock_session().await?;
        let project_key = advisory_key(&format!("enx-rag-project-v1:{project}"));
        project_lock.acquire(project_key).await?;
        let db = self.db().await?;
        let removed = db
            .execute(
                &format!("DELETE FROM {} WHERE project_id = $1", table(self.dim)),
                &[&project],
            )
            .await?;
        project_lock.release(project_key).await?;
        drop(project_lock);
        Ok(format!("Removed {removed} chunks of {}.", root.display()))
    }
}

fn render(value: &Value) -> String {
    super::render(value)
}

/// The vector extension, the table and its indexes, created when missing. A
/// managed database where the extension already exists but this role may
/// not create it is fine.
async fn prepare(db: &tokio_postgres::Client, dim: usize) -> Result<()> {
    if let Err(error) = db
        .batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
        .await
    {
        let present = db
            .query_opt("SELECT 1 FROM pg_extension WHERE extname = 'vector'", &[])
            .await?
            .is_some();
        if !present {
            bail!(
                "pgvector is not installed in this database and this role cannot add it: {error}"
            );
        }
    }
    let t = table(dim);
    db.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {t} (
           project_id TEXT NOT NULL,
           id TEXT NOT NULL,
           source_file TEXT NOT NULL,
           start_line INTEGER NOT NULL,
           end_line INTEGER NOT NULL,
           content TEXT NOT NULL,
           content_hash TEXT NOT NULL,
           embedding vector({dim}) NOT NULL,
           embed_model TEXT NOT NULL DEFAULT '',
           content_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED,
           indexed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
           PRIMARY KEY (project_id, id));
         ALTER TABLE {t} ADD COLUMN IF NOT EXISTS embed_model TEXT NOT NULL DEFAULT '';
         CREATE INDEX IF NOT EXISTS {t}_project ON {t} (project_id, embed_model);
         CREATE INDEX IF NOT EXISTS {t}_tsv ON {t} USING GIN (content_tsv);"
    ))
    .await
    .context("creating the RAG table")?;
    if dim <= HNSW_MAX {
        db.batch_execute(&format!(
            "CREATE INDEX IF NOT EXISTS {t}_embedding ON {t} USING hnsw (embedding vector_cosine_ops);"
        ))
        .await
        .context("creating the vector index")?;
    }
    Ok(())
}

/// A vector as pgvector's text form, every float at full precision.
fn vector_literal(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 12);
    out.push('[');
    for (i, value) in vector.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    out
}

/// The id a folder is indexed under: its name, and a short hash of its full
/// path so two checkouts named alike do not share chunks.
pub fn project_id(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}-{:08x}", stable_hash(&root.to_string_lossy()) as u32)
}

/// A hash that is the same on every run and every machine.
fn stable_hash(text: &str) -> u64 {
    // FNV-1a: tiny, stable, and change detection needs nothing stronger.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// One chunk of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub id: String,
    pub file: String,
    /// 1-based, inclusive.
    pub start: usize,
    pub end: usize,
    /// Where it sits: `impl Rag`, `Install > macOS`; empty at the top.
    pub scope: String,
    /// What it defines, by name.
    pub names: Vec<String>,
    pub body: String,
    /// Of `text()`: the same content in the same place hashes the same.
    pub hash: String,
}

impl Chunk {
    pub fn new(
        file: &str,
        start: usize,
        end: usize,
        scope: &str,
        names: Vec<String>,
        body: String,
    ) -> Self {
        let mut chunk = Self {
            id: String::new(),
            file: file.to_owned(),
            start,
            end,
            scope: scope.to_owned(),
            names,
            body,
            hash: String::new(),
        };
        chunk.hash = format!("{:016x}", stable_hash(&chunk.text()));
        chunk
    }

    /// What is embedded and stored: where the chunk is and what it defines,
    /// then its lines. No line numbers: code that only moved keeps its
    /// embedding, and its range is updated on its own.
    fn text(&self) -> String {
        let mut head = format!("File: {}", self.file);
        if !self.scope.is_empty() {
            head.push_str(&format!("\nIn: {}", self.scope));
        }
        if !self.names.is_empty() {
            let names: Vec<&str> = self.names.iter().take(12).map(String::as_str).collect();
            head.push_str(&format!("\nDefines: {}", names.join(", ")));
        }
        format!("{head}\n\n{}", self.body)
    }
}

/// Files worth indexing: source, config and prose. Lockfiles, minified
/// bundles and anything that looks like a secret are left out.
fn wanted(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.starts_with(".env")
        || name.ends_with(".lock")
        || name == "package-lock.json"
        || name == "pnpm-lock.yaml"
        || name.contains(".min.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
    {
        return false;
    }
    if matches!(
        name.as_str(),
        "dockerfile" | "makefile" | "justfile" | "readme"
    ) {
        return true;
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "rs" | "go"
            | "py"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "mjs"
            | "cjs"
            | "vue"
            | "svelte"
            | "astro"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "cs"
            | "rb"
            | "php"
            | "scala"
            | "dart"
            | "lua"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "ml"
            | "clj"
            | "sql"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "html"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "proto"
            | "graphql"
            | "gql"
            | "tf"
            | "md"
            | "mdx"
            | "txt"
            | "rst"
    )
}

/// Every chunk of every wanted file under `root`, in the order git would see
/// the files (`.gitignore` and hidden files skipped).
/// Lower the priority of indexing so it yields the CPU to the session and
/// the rest of the machine. On Linux a thread's niceness is its own, so this
/// runs on each thread that walks and cuts files; on macOS it covers the
/// process; on Windows the process drops to below-normal.
fn lower_priority() {
    #[cfg(unix)]
    {
        // SAFETY: setpriority only reads its arguments; it changes this
        // process's (or thread's) scheduling priority and nothing in memory.
        unsafe {
            nix::libc::setpriority(nix::libc::PRIO_PROCESS, 0, 10);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
        };
        // SAFETY: the pseudo-handle of the current process is always valid.
        unsafe {
            SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        }
    }
}

pub fn collect(root: &Path) -> Result<Vec<Chunk>> {
    let rels: Vec<String> = files(root).into_keys().collect();
    Ok(chunks_of(root, &rels))
}

/// Every wanted file under `root`, by its path relative to it (with `/`),
/// and its stamp.
fn files(root: &Path) -> BTreeMap<String, (PathBuf, Stamp)> {
    let walk = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .build();
    let mut files = BTreeMap::new();
    for entry in walk.flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) || !wanted(path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_FILE || meta.len() == 0 {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        files.insert(
            rel,
            (path.to_path_buf(), (meta.len(), meta.modified().ok())),
        );
    }
    files
}

/// The chunks of the files `rels` under `root`. One that is not UTF-8, or
/// is binary, is not text to search.
fn chunks_of(root: &Path, rels: &[String]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for rel in rels {
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        if text.contains('\0') {
            continue;
        }
        chunks.extend(chunk_file(rel, &text));
    }
    chunks
}

pub use chunk::chunk_file;

#[async_trait::async_trait]
impl Server for Rag {
    /// The hook that keeps the index fresh with no one asking: the
    /// workspace is synced as soon as the server starts (the session
    /// opening), then every half minute whatever changed, by the agent or
    /// by hand. Off with `auto_index = false`.
    fn start(self: Arc<Self>) {
        if !self.setup.auto_index() {
            return;
        }
        lower_priority();
        tokio::spawn(async move {
            let mut first = true;
            loop {
                if !first {
                    tokio::select! {
                        _ = self.wake.notified() => {
                            // Edits come in bursts (a multi-file change):
                            // gathered, they are one sync, not several.
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                }
                first = false;
                let mut outcome = Ok(());
                // What the session just changed goes first, turn or not:
                // one file is cheap, and the next search should see it.
                let queued: Vec<String> = self
                    .queued
                    .lock()
                    .map(|mut q| std::mem::take(&mut *q).into_iter().collect())
                    .unwrap_or_default();
                if !queued.is_empty() {
                    outcome = self.sync(&self.workspace, Some(queued)).await.map(|_| ());
                }
                // The full scan (a walk of every file) waits for the turn to
                // end; a search still refreshes on its own before it answers.
                if !self.busy() {
                    let scanned = self.refresh(&self.workspace).await.map(|_| ());
                    outcome = outcome.and(scanned);
                }
                if let Ok(mut last) = self.last_error.lock() {
                    *last = outcome.err().map(|e| format!("{e:#}"));
                }
            }
        });
    }

    /// `notifications/enx/changed` with `files` (paths as the agent wrote
    /// them): index those now. `notifications/enx/turn` with `busy`: a turn
    /// began or ended.
    fn notified(&self, method: &str, params: &Value) {
        match method {
            "notifications/enx/changed" => {
                let files: Vec<String> = params
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|files| {
                        files
                            .iter()
                            .filter_map(Value::as_str)
                            .filter_map(|path| self.relative(path))
                            .collect()
                    })
                    .unwrap_or_default();
                if files.is_empty() || !self.setup.auto_index() {
                    return;
                }
                if let Ok(mut queued) = self.queued.lock() {
                    queued.extend(files);
                }
                self.wake.notify_one();
            }
            "notifications/enx/turn" => {
                let busy = params.get("busy").and_then(Value::as_bool).unwrap_or(false);
                if let Ok(mut since) = self.busy_since.lock() {
                    *since = busy.then(std::time::Instant::now);
                }
                if !busy {
                    // The scan put off during the turn runs now.
                    self.wake.notify_one();
                }
            }
            _ => {}
        }
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let path = ("path", "The folder, absolute; the workspace when left out");
        vec![
            ToolSpec {
                name: "index",
                description: "Index the project now and say what changed. Usually not needed: the workspace is indexed when the session starts and kept fresh by itself (every half minute, and before each search). Call it for another folder, after a huge change you want searchable at once, or when status shows auto-indexing is off.",
                input_schema: schema(&[path], &[]),
            },
            ToolSpec {
                name: "search",
                description: "Find the code that answers a question, by meaning and by words: \"where are refunds issued\", \"how is the session token checked\". Returns the best chunks with their file and line range, best first. Use it before reading files one by one in a large project.",
                input_schema: schema(
                    &[
                        ("query", "What you are looking for, in plain words"),
                        ("limit", "How many chunks, default 8, at most 30"),
                        path,
                    ],
                    &["query"],
                ),
            },
            ToolSpec {
                name: "status",
                description: "Whether the project is indexed: chunks, files, when it was last indexed, the model, whether auto-indexing is on or running now, and its last error.",
                input_schema: schema(&[path], &[]),
            },
            ToolSpec {
                name: "forget",
                description: "Delete the project's index.",
                input_schema: schema(&[path], &[]),
            },
        ]
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<String> {
        match tool {
            "index" => self.index(args).await,
            "search" => self.search(args).await,
            "status" => self.status(args).await,
            "forget" => self.forget(args).await,
            other => bail!("rag has no tool `{other}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rag_in(workspace: &Path) -> Rag {
        let mut rag = Rag::new(
            DatabaseBackend::Postgres,
            Some("postgres://localhost/none"),
            "",
            &RagSetup::default(),
        )
        .unwrap();
        rag.workspace = std::fs::canonicalize(workspace).unwrap();
        rag
    }

    /// The hook after `write`/`edit`: a changed file is queued by its path
    /// relative to the workspace, however the agent wrote it, and wakes the
    /// background loop. A path outside the workspace is not.
    #[test]
    fn a_changed_file_is_queued_relative_to_the_workspace() {
        let dir = std::env::temp_dir().join(format!("enx-rag-hook-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        let rag = rag_in(&dir);
        let absolute = dir.join("src/a.rs").to_string_lossy().into_owned();
        rag.notified(
            "notifications/enx/changed",
            &json!({ "files": [absolute, "src/gone.rs", "/elsewhere/x.rs"] }),
        );
        let queued: Vec<String> = rag.queued.lock().unwrap().iter().cloned().collect();
        assert_eq!(queued, ["src/a.rs", "src/gone.rs"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_turn_marks_the_session_busy_until_it_ends() {
        let rag = rag_in(&std::env::temp_dir());
        assert!(!rag.busy());
        rag.notified("notifications/enx/turn", &json!({ "busy": true }));
        assert!(rag.busy());
        rag.notified("notifications/enx/turn", &json!({ "busy": false }));
        assert!(!rag.busy());
    }

    #[test]
    fn with_auto_index_off_nothing_is_queued() {
        let dir = std::env::temp_dir();
        let setup = RagSetup {
            auto_index: Some(false),
            ..Default::default()
        };
        let mut rag = Rag::new(
            DatabaseBackend::Postgres,
            Some("postgres://localhost/none"),
            "",
            &setup,
        )
        .unwrap();
        rag.workspace = std::fs::canonicalize(&dir).unwrap();
        rag.notified("notifications/enx/changed", &json!({ "files": ["x.rs"] }));
        assert!(rag.queued.lock().unwrap().is_empty());
    }

    #[test]
    fn each_provider_has_its_defaults() {
        let voyage = RagSetup::default();
        assert_eq!(voyage.provider(), Provider::Voyage);
        assert_eq!(voyage.model(), MODEL);
        assert_eq!(voyage.dimension(), DIM);
        assert_eq!(voyage.reranker().as_deref(), Some(RERANK_MODEL));
        assert_eq!(voyage.base_url(), VOYAGE);
        voyage.check().unwrap();

        let openai = RagSetup {
            provider: "openai".into(),
            ..Default::default()
        };
        assert_eq!(openai.model(), "text-embedding-3-small");
        assert_eq!(openai.dimension(), 1536);
        assert_eq!(openai.reranker(), None, "OpenAI has no reranker");

        let large = RagSetup {
            provider: "openai".into(),
            model: "text-embedding-3-large".into(),
            ..Default::default()
        };
        assert_eq!(large.dimension(), 1024);
    }

    #[test]
    fn a_custom_endpoint_needs_its_url_model_and_width() {
        let mut custom = RagSetup {
            provider: "custom".into(),
            ..Default::default()
        };
        assert!(custom.check().is_err(), "no URL");
        custom.base_url = "http://localhost:11434/v1/".into();
        assert!(custom.check().is_err(), "no model");
        custom.model = "nomic-embed-text".into();
        assert!(custom.check().is_err(), "no width");
        custom.dimension = 768;
        custom.check().unwrap();
        assert_eq!(custom.base_url(), "http://localhost:11434/v1");
        assert_eq!(custom.reranker(), None);
        custom.rerank = "jina-reranker-v2".into();
        assert_eq!(custom.reranker().as_deref(), Some("jina-reranker-v2"));
    }

    #[test]
    fn the_reranker_can_be_turned_off() {
        let setup = RagSetup {
            rerank: "off".into(),
            ..Default::default()
        };
        assert_eq!(setup.reranker(), None);
    }

    /// A change of model or width changes what chunks record, so `index`
    /// knows to embed again.
    #[test]
    fn the_embedder_names_provider_model_and_width() {
        let a = RagSetup::default();
        let b = RagSetup {
            dimension: 512,
            ..Default::default()
        };
        assert_eq!(a.embedder(), "voyage:voyage-code-3:1024");
        assert_ne!(a.embedder(), b.embedder());
    }

    /// A setup saved before providers existed still reads, as Voyage.
    #[test]
    fn an_old_setup_still_reads() {
        let old: RagSetup = serde_json::from_str("{}").unwrap();
        assert_eq!(old, RagSetup::default());
        assert_eq!(serde_json::to_string(&RagSetup::default()).unwrap(), "{}");
    }

    #[test]
    fn backend_resolution_preserves_legacy_dsn_and_supported_default() {
        let setup = RagSetup::default();
        let expected = if DatabaseBackend::embedded_supported() {
            DatabaseBackend::Embedded
        } else {
            DatabaseBackend::Postgres
        };
        assert_eq!(setup.effective_database_backend(false).unwrap(), expected);
        assert_eq!(
            setup.effective_database_backend(true).unwrap(),
            DatabaseBackend::Postgres
        );
        let explicit = RagSetup {
            database: Some(DatabaseBackend::Embedded),
            ..Default::default()
        };
        assert_eq!(
            explicit.effective_database_backend(true).is_ok(),
            DatabaseBackend::embedded_supported()
        );
    }

    #[test]
    fn backend_serializes_lowercase_and_round_trips() {
        let setup = RagSetup {
            database: Some(DatabaseBackend::Embedded),
            ..Default::default()
        };
        let encoded = serde_json::to_string(&setup).unwrap();
        assert!(encoded.contains("\"database\":\"embedded\""));
        assert_eq!(serde_json::from_str::<RagSetup>(&encoded).unwrap(), setup);
    }

    #[test]
    fn secrets_lockfiles_and_bundles_are_not_indexed() {
        for path in [
            ".env",
            ".env.local",
            "Cargo.lock",
            "package-lock.json",
            "app.min.js",
            "id.pem",
        ] {
            assert!(!wanted(Path::new(path)), "{path}");
        }
        for path in [
            "src/main.rs",
            "web/App.tsx",
            "README.md",
            "Dockerfile",
            "schema.sql",
        ] {
            assert!(wanted(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn the_walk_honours_gitignore() {
        let dir = std::env::temp_dir().join(format!("enx-rag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        // `ignore` honours .gitignore inside a git repository.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(dir.join("build/out.js"), "var a = 1;\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let chunks = collect(&dir).unwrap();
        let files: Vec<&str> = chunks.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(files, vec!["src/lib.rs"], "{files:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_vector_keeps_its_precision() {
        let literal = vector_literal(&[0.1, -2.5e-7, 1.0]);
        assert_eq!(literal, "[0.1,-0.00000025,1]");
    }

    #[test]
    fn two_checkouts_with_one_name_are_two_projects() {
        assert_ne!(
            project_id(Path::new("/a/shop")),
            project_id(Path::new("/b/shop"))
        );
        assert!(project_id(Path::new("/a/shop")).starts_with("shop-"));
    }
}
