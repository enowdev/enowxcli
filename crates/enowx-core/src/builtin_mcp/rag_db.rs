//! Shared owner for the persistent embedded PostgreSQL RAG database.
//!
//! Client processes serialize startup and shutdown with advisory lock files;
//! the daemon owns the Oliphaunt server until it has been idle for one minute.

use std::{
    fs::{self, File, OpenOptions},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, NoTls};

use crate::config::{atomic_write, home_dir};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_READY_TIMEOUT: Duration = Duration::from_secs(25);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const IDENTITY: &str = "enowx-rag-db-v1";

#[derive(Debug, Serialize, Deserialize)]
struct EndpointMetadata {
    identity: String,
    version: u32,
    endpoint: String,
}

/// Keeps concurrent clients from racing owner startup or idle shutdown until
/// the caller has connected to PostgreSQL and started its driver task.
pub(crate) struct StartupLease {
    _lock: File,
}

fn database_dir() -> PathBuf {
    home_dir().join("rag-db")
}

fn cluster_dir() -> PathBuf {
    database_dir().join("cluster")
}

fn metadata_path() -> PathBuf {
    database_dir().join("endpoint.json")
}

fn lock_path(name: &str) -> PathBuf {
    database_dir().join(name)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

async fn ensure_dirs() -> Result<()> {
    let root = database_dir();
    tokio::task::spawn_blocking(move || {
        ensure_private_dir(&root)?;
        ensure_private_dir(&cluster_dir())
    })
    .await
    .context("preparing embedded database directories")??;
    Ok(())
}

async fn acquire_lock(name: &'static str, timeout: Option<Duration>) -> Result<File> {
    let path = lock_path(name);
    let started = Instant::now();
    loop {
        let path_for_open = path.clone();
        let attempt = tokio::task::spawn_blocking(move || -> Result<Option<File>> {
            let file = open_lock(&path_for_open)?;
            match file.try_lock_exclusive() {
                Ok(()) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .await
        .context("acquiring embedded database lock")??;
        if let Some(file) = attempt {
            return Ok(file);
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            bail!("timed out waiting for embedded RAG database startup lock");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn try_lock(name: &'static str) -> Result<Option<File>> {
    let path = lock_path(name);
    tokio::task::spawn_blocking(move || {
        let file = open_lock(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    })
    .await
    .context("checking embedded database owner lock")?
}

async fn connect(endpoint: &str) -> Result<Client> {
    let (client, connection) = tokio_postgres::connect(endpoint, NoTls)
        .await
        .with_context(|| format!("connecting to embedded PostgreSQL endpoint {endpoint}"))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "embedded PostgreSQL connection closed");
        }
    });
    Ok(client)
}

fn canonical_cluster() -> Result<PathBuf> {
    fs::canonicalize(cluster_dir().join("pgdata"))
        .context("resolving embedded PostgreSQL data directory")
}

async fn verifies_cluster(endpoint: &str) -> Result<bool> {
    let Ok(client) = connect(endpoint).await else {
        return Ok(false);
    };
    let row = match client
        .query_one("SELECT current_setting('data_directory')", &[])
        .await
    {
        Ok(row) => row,
        Err(_) => return Ok(false),
    };
    let actual: String = row.get(0);
    let matches = tokio::task::spawn_blocking(move || {
        let actual = fs::canonicalize(actual)?;
        let expected = canonical_cluster()?;
        Ok::<_, anyhow::Error>(actual == expected)
    })
    .await
    .context("resolving PostgreSQL data directory")??;
    Ok(matches)
}

async fn read_metadata() -> Result<Option<EndpointMetadata>> {
    let path = metadata_path();
    tokio::task::spawn_blocking(move || {
        match fs::read(&path) {
            Ok(bytes) => {
                let metadata: EndpointMetadata = serde_json::from_slice(&bytes)
                    .with_context(|| format!("reading {}", path.display()))?;
                if metadata.identity != IDENTITY || metadata.version != 1 {
                    bail!("embedded RAG database endpoint metadata has an unsupported identity or version");
                }
                Ok(Some(metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    })
    .await
    .context("reading embedded database endpoint metadata")?
}

async fn remove_metadata() -> Result<()> {
    let path = metadata_path();
    tokio::task::spawn_blocking(move || match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    })
    .await
    .context("removing stale endpoint metadata")?
}

async fn wait_for_owner(timeout: Duration) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(metadata) = read_metadata().await? {
            if verifies_cluster(&metadata.endpoint).await? {
                return Ok(metadata.endpoint);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("embedded RAG database owner did not publish a verifiable endpoint in time");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Start or find the shared server. The returned lease must be kept until the
/// caller has opened its PostgreSQL connection and started its driver.
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_vendor = "apple", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "windows", target_env = "msvc")
))]
pub(crate) async fn ensure_server() -> Result<(String, StartupLease)> {
    ensure_dirs().await?;
    let startup = acquire_lock("startup.lock", Some(STARTUP_TIMEOUT)).await?;
    if let Some(metadata) = read_metadata().await? {
        if verifies_cluster(&metadata.endpoint).await? {
            if try_lock("owner.lock").await?.is_some() {
                bail!("embedded PostgreSQL is still running without its enowx owner");
            }
            return Ok((metadata.endpoint, StartupLease { _lock: startup }));
        }
    }
    if try_lock("owner.lock").await?.is_none() {
        let endpoint = wait_for_owner(CLIENT_READY_TIMEOUT).await?;
        return Ok((endpoint, StartupLease { _lock: startup }));
    }

    remove_metadata().await?;
    let executable = std::env::current_exe().context("locating enowx executable")?;
    let mut command = tokio::process::Command::new(executable);
    command.args(["mcp", "serve", "rag-db"]);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().creation_flags(0x00000008 | 0x00000200);
    }
    command
        .spawn()
        .context("starting embedded RAG database owner")?;
    let endpoint = wait_for_owner(CLIENT_READY_TIMEOUT).await?;
    Ok((endpoint, StartupLease { _lock: startup }))
}

#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_vendor = "apple", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "windows", target_env = "msvc")
)))]
pub(crate) async fn ensure_server() -> Result<(String, StartupLease)> {
    bail!("embedded RAG database is unsupported on this target; select PostgreSQL instead")
}

#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_vendor = "apple", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "windows", target_env = "msvc")
))]
pub async fn serve() -> Result<()> {
    ensure_dirs().await?;
    let owner = acquire_lock("owner.lock", None).await?;
    let root = database_dir();
    let cluster = cluster_dir();
    tokio::task::spawn_blocking(move || {
        ensure_private_dir(&root)?;
        ensure_private_dir(&cluster)
    })
    .await
    .context("securing embedded database directories")??;
    let root = database_dir();
    let listen = tokio::task::spawn_blocking(move || local_listen(&root))
        .await
        .context("preparing local PostgreSQL listener")??;
    let cluster = cluster_dir();
    let runtime = tokio::runtime::Handle::current();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("enowx-rag-db-startup".to_owned())
        .spawn(move || {
            let started = catch_unwind(AssertUnwindSafe(|| {
                runtime.block_on(
                    oliphaunt::AsyncOliphauntServer::builder()
                        .storage(oliphaunt::DatabaseStorage::Directory(cluster))
                        .extension(oliphaunt_extension_vector::VECTOR)
                        .listen(listen)
                        .start(),
                )
            }));
            let result = match started {
                Ok(Ok(server)) => Ok(server),
                Ok(Err(error)) => Err(anyhow!("starting embedded PostgreSQL: {error}")),
                Err(_) => Err(anyhow!("embedded PostgreSQL startup panicked")),
            };
            if let Err(mpsc::SendError((result, owner))) = sender.send((result, owner)) {
                match result {
                    Ok(server) => match runtime.block_on(server.close()) {
                        Ok(()) => drop(owner),
                        Err(error) => {
                            std::mem::forget(owner);
                            std::mem::forget(server);
                            tracing::error!(%error, "could not close late embedded PostgreSQL startup; retaining owner lock");
                            runtime.block_on(std::future::pending::<()>());
                        }
                    },
                    Err(_) => drop(owner),
                }
            }
        })
        .context("spawning embedded PostgreSQL startup thread")?;
    let received =
        tokio::task::spawn_blocking(move || match receiver.recv_timeout(SERVER_START_TIMEOUT) {
            Ok(result) => Ok((result, false)),
            Err(mpsc::RecvTimeoutError::Timeout) => receiver
                .recv()
                .map(|result| (result, true))
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            Err(error) => Err(error),
        })
        .await
        .context("waiting for embedded PostgreSQL startup result")??;
    let (started, owner) = received.0;
    if received.1 {
        match started {
            Ok(server) => {
                return close_or_retain(
                    server,
                    owner,
                    anyhow!("embedded PostgreSQL startup exceeded 20 seconds"),
                )
                .await;
            }
            Err(error) => {
                return Err(error.context("embedded PostgreSQL startup exceeded 20 seconds"))
            }
        }
    }
    let server = match started {
        Ok(server) => server,
        Err(error) => return Err(error),
    };
    let endpoint = server.connection_string().to_owned();
    let initialize = async {
        let client = connect(&endpoint).await?;
        client
            .batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
            .await
            .context("initializing pgvector extension")?;
        Ok::<(), anyhow::Error>(())
    };
    match tokio::time::timeout(SERVER_START_TIMEOUT, initialize).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return close_or_retain(server, owner, error).await,
        Err(_) => {
            return close_or_retain(
                server,
                owner,
                anyhow!("pgvector initialization exceeded 20 seconds"),
            )
            .await;
        }
    }

    let metadata = EndpointMetadata {
        identity: IDENTITY.to_owned(),
        version: 1,
        endpoint: endpoint.clone(),
    };
    let path = metadata_path();
    let bytes = serde_json::to_vec(&metadata)?;
    let publish = tokio::task::spawn_blocking(move || {
        atomic_write(&path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("publishing embedded database endpoint");
    match publish {
        Ok(Ok(())) => {}
        Ok(Err(error)) | Err(error) => return close_or_retain(server, owner, error).await,
    }
    match monitor(&server, &endpoint).await {
        Ok(()) => Ok(()),
        Err(error) => close_or_retain(server, owner, error).await,
    }
}
#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_vendor = "apple", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "windows", target_env = "msvc")
)))]
pub async fn serve() -> Result<()> {
    bail!("embedded RAG database owner is unsupported on this target; select PostgreSQL instead")
}

#[cfg(all(
    unix,
    any(
        all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
        all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
        all(target_arch = "aarch64", target_vendor = "apple", target_os = "macos")
    )
))]
fn local_listen(_root: &Path) -> Result<oliphaunt::ServerListen> {
    use std::os::unix::fs::PermissionsExt;
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in home_dir().to_string_lossy().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    let socket_dir = std::env::temp_dir().join(format!("enx-rag-{hash:016x}"));
    ensure_private_dir(&socket_dir)?;
    fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))?;
    let socket_dir = socket_dir
        .to_str()
        .context("temporary socket path is not UTF-8")?;
    Ok(oliphaunt::ServerListen::unix(socket_dir))
}

#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
fn local_listen(_root: &Path) -> Result<oliphaunt::ServerListen> {
    // tcp() binds an ephemeral loopback address. Windows uses trust auth, so
    // any local process able to reach the port can access the RAG database.
    Ok(oliphaunt::ServerListen::tcp())
}
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_vendor = "apple", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "windows", target_env = "msvc")
))]
async fn close_or_retain(
    server: oliphaunt::AsyncOliphauntServer,
    owner: File,
    error: anyhow::Error,
) -> Result<()> {
    match server.close().await {
        Ok(()) => {
            drop(owner);
            Err(error)
        }
        Err(close_error) => {
            std::mem::forget(owner);
            std::mem::forget(server);
            tracing::error!(%close_error, "could not close embedded PostgreSQL; retaining owner process and lock");
            std::future::pending::<()>().await;
            unreachable!()
        }
    }
}

async fn monitor(server: &oliphaunt::AsyncOliphauntServer, endpoint: &str) -> Result<()> {
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
        tokio::time::sleep(IDLE_POLL).await;
        let client = match connect(&endpoint).await {
            Ok(client) => client,
            Err(error) => return Err(error.context("monitoring embedded PostgreSQL")),
        };
        let row = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()",
                &[],
            )
            .await
            .context("checking embedded PostgreSQL clients")?;
        let clients: i64 = row.get(0);
        drop(client);
        if clients == 0 {
            let since = idle_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= IDLE_TIMEOUT {
                let startup = acquire_lock("startup.lock", None).await?;
                let client = connect(&endpoint).await?;
                let row = client
                    .query_one(
                        "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()",
                        &[],
                    )
                    .await?;
                let clients: i64 = row.get(0);
                drop(client);
                if clients == 0 {
                    server
                        .close()
                        .await
                        .context("closing embedded PostgreSQL")?;
                    remove_metadata().await?;
                    drop(startup);
                    return Ok(());
                }
                idle_since = None;
            }
        } else {
            idle_since = None;
        }
    }
}
