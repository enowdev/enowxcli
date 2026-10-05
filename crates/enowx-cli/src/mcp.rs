//! `enx mcp` and `enx vps`: install the MCP servers built into enx, and the
//! VPSes the `vps` server reaches.

use std::io::{BufRead as _, IsTerminal as _, Write as _};

use crate::auth::read_key;
use anyhow::{bail, Context as _, Result};
use enowx_core::{
    auth::Auth,
    builtin_mcp::{self, rag, secret_id, vps, BuiltinConfig, Endpoint},
};

/// A visible answer, typed or piped.
fn read_line(prompt: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// `enx mcp list`: each built-in server and whether it is installed.
pub fn list() -> Result<()> {
    let config = BuiltinConfig::load()?;
    let auth = Auth::load()?;
    for name in builtin_mcp::NAMES {
        let detail = match name {
            "coolify" => config.coolify.as_ref().map(|e| e.base_url.clone()),
            "dokploy" => config.dokploy.as_ref().map(|e| e.base_url.clone()),
            "rag" => config.rag.as_ref().map(|setup| {
                let has_dsn = auth.is_stored(&builtin_mcp::rag_dsn_id());
                let backend = setup
                    .effective_database_backend(has_dsn)
                    .unwrap_or(rag::DatabaseBackend::Postgres);
                format!(
                    "{} + {}",
                    setup.provider().label(),
                    match backend {
                        rag::DatabaseBackend::Embedded => "embedded PostgreSQL",
                        rag::DatabaseBackend::Postgres => "PostgreSQL",
                    }
                )
            }),
            _ => Some(format!(
                "{} VPS: {}",
                config.vps.len(),
                config.vps.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
        };
        let on = enowx_core::persist::read_overrides()
            .get(name)
            .and_then(|o| o.enabled)
            .unwrap_or(false);
        match detail {
            Some(detail) => println!(
                "{name:8} {:<13} {detail}",
                if on {
                    "configured, on"
                } else {
                    "configured, off"
                }
            ),
            None => println!("{name:8} {:<13} (enx {})", "not set up", install_hint(name)),
        }
    }
    Ok(())
}

fn install_hint(name: &str) -> &'static str {
    match name {
        "vps" => "vps add <name> --host <address> --user <user>",
        "coolify" => "mcp set coolify --url <url> --token <token>",
        "rag" => {
            "mcp set rag [--database embedded|postgres] [--dsn <postgres://...>] [--token <key>]"
        }
        _ => "mcp set dokploy --url <url> --token <token>",
    }
}

/// `enx mcp install coolify|dokploy`: the panel's URL and an API token.
pub fn install(name: &str, url: Option<String>, token: Option<String>) -> Result<()> {
    let (label, token_hint) = match name {
        "coolify" => (
            "Coolify",
            "Create one in Coolify under Keys & Tokens > API tokens.",
        ),
        "dokploy" => (
            "Dokploy",
            "Create one in Dokploy under Settings > Profile > API/CLI.",
        ),
        "vps" => bail!(
            "VPSes are added one at a time: enx vps add <name> --host <address> --user <user>"
        ),
        other => bail!(
            "enx has no built-in MCP server `{other}` (built in: {})",
            builtin_mcp::NAMES.join(", ")
        ),
    };
    let url = match url {
        Some(url) => url,
        None => read_line(&format!("{label} URL (https://...): "))?,
    };
    let url = url.trim().trim_end_matches('/').to_owned();
    anyhow::ensure!(
        url.starts_with("https://") || url.starts_with("http://"),
        "the {label} URL must start with https:// or http://"
    );
    let token = match token {
        Some(token) => token,
        None => {
            eprintln!("{token_hint}");
            read_key(&format!("{label} API token: "))?
        }
    };
    anyhow::ensure!(!token.trim().is_empty(), "no token entered; nothing saved");

    let mut config = BuiltinConfig::load()?;
    let endpoint = Some(Endpoint {
        base_url: url.clone(),
    });
    match name {
        "coolify" => config.coolify = endpoint,
        _ => config.dokploy = endpoint,
    }
    let mut auth = Auth::load()?;
    auth.store(&secret_id(name), &token)?;
    let path = config.save()?;
    // Turn it on: a built-in server is off until configured, and filling it
    // in from the CLI is how the user (or an agent helping them) sets it up.
    enowx_core::persist::set_mcp_enabled(name, true)?;
    println!(
        "Configured {name} ({url}) and turned it on. Setup in {}",
        path.display()
    );
    Ok(())
}

/// `enx mcp set rag`: the selected database and embedding setup. Secrets
/// remain in `auth.json`.
pub struct RagArgs {
    pub database: Option<String>,
    pub dsn: Option<String>,
    pub token: Option<String>,
    pub url: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub dimension: Option<usize>,
    pub rerank: Option<String>,
    pub auto_index: Option<String>,
}

/// `enx mcp set rag`: the database, the embedding provider and its key, the
/// model, width and reranker. What is left out keeps its current value; a
/// new provider starts on its own defaults.
pub fn set_rag(args: RagArgs) -> Result<()> {
    let requested_backend = args
        .database
        .as_deref()
        .map(|database| match database {
            "embedded" => Ok(rag::DatabaseBackend::Embedded),
            "postgres" => Ok(rag::DatabaseBackend::Postgres),
            _ => bail!("database must be embedded or postgres"),
        })
        .transpose()?;
    anyhow::ensure!(
        !(requested_backend == Some(rag::DatabaseBackend::Embedded) && args.dsn.is_some()),
        "--database embedded cannot be used with --dsn"
    );
    let mut auth = Auth::load()?;
    let mut config = BuiltinConfig::load()?;
    let mut setup = config.rag.clone().unwrap_or_default();
    let stored_dsn = auth
        .key(&builtin_mcp::rag_dsn_id(), &[])
        .map(|(value, _)| value);
    let backend = if let Some(backend) = requested_backend {
        backend
    } else if args.dsn.is_some() {
        rag::DatabaseBackend::Postgres
    } else {
        setup.effective_database_backend(stored_dsn.is_some())?
    };
    anyhow::ensure!(
        backend != rag::DatabaseBackend::Embedded || rag::DatabaseBackend::embedded_supported(),
        "embedded RAG is not supported on this target"
    );
    setup.database = Some(backend);
    if let Some(provider) = &args.provider {
        let provider = rag::Provider::parse(provider);
        if provider != setup.provider() {
            setup = rag::RagSetup {
                provider: provider.id().into(),
                ..Default::default()
            };
        }
    }
    if let Some(url) = args.url {
        setup.base_url = url.trim().trim_end_matches('/').to_owned();
    }
    if let Some(model) = args.model {
        setup.model = model.trim().to_owned();
        if args.dimension.is_none() {
            setup.dimension = 0;
        }
    }
    if let Some(dimension) = args.dimension {
        setup.dimension = dimension;
    }
    if let Some(rerank) = args.rerank {
        setup.rerank = rerank.trim().to_owned();
    }
    if let Some(auto) = args.auto_index {
        setup.auto_index = (auto == "off").then_some(false);
    }
    setup.database = Some(backend);
    setup.check()?;
    let provider = setup.provider();

    let dsn_label = if backend == rag::DatabaseBackend::Postgres {
        let dsn = match args.dsn {
            Some(dsn) => Some(dsn),
            None if stored_dsn.is_some() => None,
            None => Some(read_key("Postgres connection string (postgres://...): ")?),
        };
        if let Some(dsn) = dsn {
            let dsn = dsn.trim().to_owned();
            anyhow::ensure!(
                dsn.starts_with("postgres://") || dsn.starts_with("postgresql://"),
                "the database must be a postgres:// connection string"
            );
            auth.store(&builtin_mcp::rag_dsn_id(), &dsn)?;
        }
        "PostgreSQL"
    } else {
        "embedded PostgreSQL"
    };
    let token = match args.token {
        Some(token) => Some(token),
        None if auth.key(&secret_id("rag"), &[]).is_some() => None,
        // A local endpoint (Ollama, LM Studio) takes no key.
        None if provider == rag::Provider::Custom => None,
        None => {
            match provider {
                rag::Provider::Voyage => {
                    eprintln!("Create a key at https://dash.voyageai.com/api-keys")
                }
                rag::Provider::OpenAi => {
                    eprintln!("Create a key at https://platform.openai.com/api-keys")
                }
                rag::Provider::Custom => {}
            }
            Some(read_key(&format!("{} API key: ", provider.label()))?)
        }
    };
    if let Some(token) = token {
        anyhow::ensure!(!token.trim().is_empty(), "no key entered; nothing saved");
        auth.store(&secret_id("rag"), token.trim())?;
    }
    config.rag = Some(setup.clone());
    config.save()?;
    enowx_core::persist::set_mcp_enabled("rag", true)?;
    println!(
        "Configured rag ({}, {}, {} dimensions, {}, reranker {}) and turned it on. Its search skill is offered to agents now.",
        provider.label(),
        setup.model(),
        setup.dimension(),
        dsn_label,
        setup.reranker().unwrap_or_else(|| "off".into())
    );
    Ok(())
}

/// `enx mcp uninstall <name>`: forget its setup and secrets.
pub fn uninstall(name: &str) -> Result<()> {
    let mut config = BuiltinConfig::load()?;
    let mut auth = Auth::load()?;
    match name {
        "coolify" => config.coolify = None,
        "dokploy" => config.dokploy = None,
        "rag" => {
            config.rag = None;
            auth.forget(&enowx_core::builtin_mcp::rag_dsn_id())?;
        }
        "vps" => {
            for host in config.vps.keys() {
                auth.forget(&vps::password_id(host))?;
            }
            config.vps.clear();
        }
        other => bail!("enx has no built-in MCP server `{other}`"),
    }
    auth.forget(&secret_id(name))?;
    config.save()?;
    enowx_core::persist::set_mcp_enabled(name, false)?;
    println!("Cleared {name} and turned it off");
    Ok(())
}

/// `enx vps add`: a VPS signing in with a key file, or a password typed now.
pub struct VpsAdd {
    pub name: String,
    pub host: Option<String>,
    pub user: Option<String>,
    pub port: u16,
    pub key: Option<String>,
    pub passphrase: Option<String>,
    pub password: Option<String>,
    pub no_password: bool,
}

pub fn vps_add(add: VpsAdd) -> Result<()> {
    let VpsAdd {
        name,
        host,
        user,
        port,
        key,
        passphrase,
        password,
        no_password,
    } = add;
    let name = name.as_str();
    anyhow::ensure!(
        vps::valid_name(name),
        "a VPS name is letters, digits, - and _ (at most 40)"
    );
    let host = host
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| name.to_owned());
    let user = user.map(|u| u.trim().to_owned()).unwrap_or_default();
    let mut auth = Auth::load()?;
    let shown = if user.is_empty() {
        host.clone()
    } else {
        format!("{user}@{host}")
    };
    // A key file, checked now rather than on the first connection: one that
    // is encrypted needs its passphrase, kept beside the password.
    let key_path = match key {
        Some(path) => {
            let expanded = if let Some(rest) = path.strip_prefix("~/") {
                std::path::PathBuf::from(
                    std::env::var("HOME")
                        .or_else(|_| std::env::var("USERPROFILE"))
                        .unwrap_or_default(),
                )
                .join(rest)
            } else {
                std::path::PathBuf::from(&path)
            };
            anyhow::ensure!(expanded.is_file(), "no key file at {path}");
            match russh_key_needs_passphrase(&expanded)? {
                false => {
                    auth.forget(&vps::passphrase_id(name))?;
                }
                true => {
                    let passphrase = match passphrase {
                        Some(p) => p,
                        None => read_key(&format!("Passphrase for {path}: "))?,
                    };
                    anyhow::ensure!(
                        enowx_core::builtin_mcp::vps::key_opens(&expanded, &passphrase),
                        "that passphrase does not open {path}; nothing saved"
                    );
                    auth.store(&vps::passphrase_id(name), &passphrase)
                        .context("storing the passphrase")?;
                }
            }
            Some(path)
        }
        None => None,
    };
    // A password is the last way in; with a key, ssh-agent or a host from
    // ~/.ssh/config there is often no need for one.
    let password = match password {
        Some(password) => Some(password),
        None if no_password || key_path.is_some() => None,
        None if !std::io::stdin().is_terminal() => None,
        None => {
            let typed = read_key(&format!(
                "Password for {shown} (Enter to sign in with ssh keys or ssh-agent only): "
            ))?;
            Some(typed).filter(|p| !p.is_empty())
        }
    };
    match &password {
        Some(password) => auth
            .store(&vps::password_id(name), password)
            .context("storing the password")?,
        None if no_password || key_path.is_some() => {
            auth.forget(&vps::password_id(name))?;
        }
        None => {}
    }
    let mut config = BuiltinConfig::load()?;
    let replaced = config
        .vps
        .insert(
            name.to_owned(),
            vps::Host {
                host: host.clone(),
                port,
                user: user.clone(),
                key_path,
            },
        )
        .is_some();
    config.save()?;
    // A configured VPS turns the vps server on.
    enowx_core::persist::set_mcp_enabled("vps", true)?;
    let saved = config.vps.get(name).expect("just inserted");
    println!(
        "{} VPS {name} ({shown}:{port}) and turned the vps server on. Signs in with: {}. Its host key is recorded on the first connection.",
        if replaced { "Updated" } else { "Added" },
        vps::sign_in_summary(name, saved, &Auth::load()?)
    );
    Ok(())
}

/// Whether a key file is encrypted. One that cannot be read at all is an
/// error now, not on the first connection.
fn russh_key_needs_passphrase(path: &std::path::Path) -> Result<bool> {
    enowx_core::builtin_mcp::vps::key_is_encrypted(path)
        .with_context(|| format!("reading the key {}", path.display()))
}

pub fn vps_list() -> Result<()> {
    let config = BuiltinConfig::load()?;
    let auth = Auth::load()?;
    if config.vps.is_empty() {
        println!("No VPS set up. Add one: enx vps add <name> --host <address or ~/.ssh/config alias> [--user <user>] [--key <file>]");
    }
    for (name, host) in &config.vps {
        let who = if host.user.is_empty() {
            host.host.clone()
        } else {
            format!("{}@{}", host.user, host.host)
        };
        println!(
            "{name:16} {who}:{}   {}",
            host.port,
            vps::sign_in_summary(name, host, &auth)
        );
    }
    Ok(())
}

pub fn vps_remove(name: &str) -> Result<()> {
    let mut config = BuiltinConfig::load()?;
    if config.vps.remove(name).is_none() {
        bail!("no VPS named {name}");
    }
    let mut auth = Auth::load()?;
    auth.forget(&vps::password_id(name))?;
    auth.forget(&vps::passphrase_id(name))?;
    config.save()?;
    println!("Removed VPS {name}");
    Ok(())
}
