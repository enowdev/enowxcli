use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use enowx_core::Config;

mod auth;
mod dev;
mod mcp;

#[derive(Debug, Parser)]
#[command(
    name = "enowx",
    version,
    about = "enowx: an open-source AI coding agent for your terminal"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the enowxcli terminal interface.
    Tui {
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Resume this session id instead of starting a new conversation.
        #[arg(long)]
        session: Option<String>,
    },
    /// Rebuild and relaunch the interface whenever a source file changes.
    Dev {
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Pin one session across restarts; defaults to the newest one.
        #[arg(long)]
        session: Option<String>,
    },
    /// Read or change ~/.enx/config.toml.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List the models enx can run, and test whether one answers. For the
    /// model-manager skill and for scripting model setup.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// The MCP servers built into enx (coolify, dokploy, vps): install them
    /// to offer them to agents.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// The VPSes the built-in `vps` MCP server reaches over SSH.
    Vps {
        #[command(subcommand)]
        command: VpsCommand,
    },
    /// Provider keys, kept in ~/.enx/auth.json.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Install the latest release in place of this enx, after checking its
    /// published checksum. With --check, only say whether there is one.
    Update {
        #[arg(long)]
        check: bool,
        /// Install the latest release even when this one is as new (to
        /// repair a binary, or to replace a build from source).
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// List the providers enx knows and which of them are connected.
    #[command(alias = "ls")]
    List,
    /// Store a provider's API key, typed at a prompt or piped in.
    Login { provider: String },
    /// Remove a provider's stored API key.
    Logout { provider: String },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Each built-in server and whether it is installed.
    #[command(alias = "ls")]
    List,
    /// Configure a built-in server: panels take a URL and token; rag selects
    /// embedded storage by default where supported, or PostgreSQL with --dsn.
    #[command(alias = "install")]
    Set {
        name: String,
        #[arg(long)]
        url: Option<String>,
        /// The API token (for rag, the Voyage AI key). Given here it is not
        /// prompted for.
        #[arg(long)]
        token: Option<String>,
        /// rag only: select embedded or postgres storage.
        #[arg(long, value_parser = ["embedded", "postgres"])]
        database: Option<String>,
        /// rag only: the Postgres connection string, local or cloud.
        #[arg(long)]
        dsn: Option<String>,
        /// rag only: where embeddings come from: voyage, openai or custom
        /// (any OpenAI-compatible endpoint, with --url).
        #[arg(long)]
        provider: Option<String>,
        /// rag only: the embedding model.
        #[arg(long)]
        model: Option<String>,
        /// rag only: the vector width the model returns.
        #[arg(long)]
        dimension: Option<usize>,
        /// rag only: the reranker, or `off`.
        #[arg(long)]
        rerank: Option<String>,
        /// rag only: index by itself on start and on change (on, the
        /// default) or only when asked (off).
        #[arg(long, value_parser = ["on", "off"])]
        auto_index: Option<String>,
    },
    /// Forget a built-in server's setup and stored secrets, and turn it off.
    #[command(alias = "uninstall")]
    Clear { name: String },
    /// Run a built-in server over stdio, as an MCP client starts it.
    #[command(hide = true)]
    Serve { name: String },
}

#[derive(Debug, Subcommand)]
enum VpsCommand {
    /// Add or update a VPS. It signs in like OpenSSH: the key file given,
    /// then ssh-agent, the keys in ~/.ssh/config or the default ~/.ssh/id_*
    /// keys, then the password. Secrets are kept in auth.json.
    Add {
        name: String,
        /// The address, or an alias from ~/.ssh/config. Defaults to the name.
        #[arg(long)]
        host: Option<String>,
        /// Defaults to the User in ~/.ssh/config, or the local user.
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// A private key file (OpenSSH, PEM or PuTTY .ppk).
        #[arg(long)]
        key: Option<String>,
        /// The key file's passphrase, when it has one; asked for if needed.
        #[arg(long)]
        passphrase: Option<String>,
        /// The password. Given here it is not prompted for.
        #[arg(long)]
        password: Option<String>,
        /// Do not ask for a password: sign in with keys and ssh-agent only.
        #[arg(long)]
        no_password: bool,
    },
    #[command(alias = "ls")]
    List,
    #[command(alias = "rm")]
    Remove { name: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print a dotted key, for example `model.active` or `provider.enowx.base_url`.
    Get { key: String },
    /// Set and persist a dotted key: `model.default deepseek/deepseek-flash`
    /// pins the model to start on, `provider.<id>.base_url <url>` adds a
    /// provider.
    Set { key: String, value: String },
    /// Print the config file path.
    Path,
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// The connected providers and the models each lists, the model in use,
    /// the per-tier models, and the per-agent models.
    #[command(alias = "ls")]
    List {
        /// One line per model as `provider/model`, nothing else, for scripts.
        #[arg(long)]
        plain: bool,
    },
    /// Make one tiny call to a model and say whether it answered, how fast,
    /// and the tokens it used. Exits non-zero when it does not answer.
    Test {
        /// `provider/model`, e.g. `deepseek/deepseek-chat`.
        model: String,
        /// The prompt to send; a short word is enough.
        #[arg(long, default_value = "Reply with: ok")]
        prompt: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // The alternate-screen TUI owns stdout, so nothing here logs to it.
    let cli = Cli::parse();
    // What a Windows update left beside the binary, once it no longer runs.
    enowx_core::update::clean_up();
    match cli.command.unwrap_or(Command::Tui {
        workspace: None,
        session: None,
    }) {
        Command::Tui { workspace, session } => {
            let mut config = Config::load()?;
            if workspace.is_some() {
                config.agent.workspace = workspace;
            }
            enowx_tui::run(config, session).await
        }
        Command::Dev { workspace, session } => dev::run(workspace, session),
        Command::Config { command } => {
            let mut config = Config::load()?;
            match command {
                ConfigCommand::Get { key } => {
                    let value = config
                        .get(&key)
                        .ok_or_else(|| anyhow::anyhow!("unknown config key: {key}"))?;
                    println!("{value}");
                }
                ConfigCommand::Set { key, value } => {
                    config.set(&key, &value)?;
                    let path = config.save()?;
                    println!("Updated {key}");
                    println!("saved {}", path.display());
                }
                ConfigCommand::Path => println!("{}", enowx_core::config::config_path().display()),
            }
            Ok(())
        }
        Command::Models { command } => models_command(command).await,
        Command::Mcp { command } => match command {
            McpCommand::List => mcp::list(),
            McpCommand::Set {
                name,
                url,
                token,
                dsn,
                database,
                provider,
                model,
                dimension,
                rerank,
                auto_index,
            } => {
                if name == "rag" {
                    mcp::set_rag(mcp::RagArgs {
                        dsn,
                        database,
                        token,
                        url,
                        provider,
                        model,
                        dimension,
                        rerank,
                        auto_index,
                    })
                } else {
                    mcp::install(&name, url, token)
                }
            }
            McpCommand::Clear { name } => mcp::uninstall(&name),
            McpCommand::Serve { name } if name == "rag-db" => {
                enowx_core::builtin_mcp::rag_db::serve().await
            }
            McpCommand::Serve { name } => enowx_core::builtin_mcp::serve(&name).await,
        },
        Command::Vps { command } => match command {
            VpsCommand::Add {
                name,
                host,
                user,
                port,
                key,
                passphrase,
                password,
                no_password,
            } => mcp::vps_add(mcp::VpsAdd {
                name,
                host,
                user,
                port,
                key,
                passphrase,
                password,
                no_password,
            }),
            VpsCommand::List => mcp::vps_list(),
            VpsCommand::Remove { name } => mcp::vps_remove(&name),
        },
        Command::Update { check, force } => update(check, force).await,
        Command::Auth { command } => {
            let mut config = Config::load()?;
            match command {
                AuthCommand::List => auth::list(&config),
                AuthCommand::Login { provider } => auth::login(&mut config, &provider)?,
                AuthCommand::Logout { provider } => auth::logout(&mut config, &provider)?,
            }
            Ok(())
        }
    }
}

/// `enx update`: the latest release in place of this one.
async fn update(check_only: bool, force: bool) -> Result<()> {
    use enowx_core::update;
    let current = update::current();
    println!("enx {current}");
    let found = if force {
        update::Check::Available(update::latest().await?)
    } else {
        update::check().await?
    };
    match found {
        update::Check::UpToDate => {
            println!("Up to date.");
            Ok(())
        }
        update::Check::Available(latest) if check_only => {
            println!("{latest} is available. Run `enx update` to install it.");
            Ok(())
        }
        update::Check::Available(latest) => {
            println!("Installing {latest}...");
            let path = update::install(&latest).await?;
            println!(
                "Installed {latest} at {}. Restart any enx that is running to use it.",
                path.display()
            );
            Ok(())
        }
    }
}

/// `enx models list` and `enx models test`.
async fn models_command(command: ModelsCommand) -> Result<()> {
    let config = Config::load()?;
    match command {
        ModelsCommand::List { plain } => {
            let detected = enowx_core::provider::detected::Detected::load();
            let connected = config.connected();
            if plain {
                for connection in &connected {
                    if let Some(listing) = detected.listing(&connection.id) {
                        for model in &listing.models {
                            println!("{}/{}", connection.id, model.id);
                        }
                    }
                }
                return Ok(());
            }
            if connected.is_empty() {
                println!("No providers connected. Add one with `enx auth login <provider>`.");
            }
            println!("Connected providers and their models:");
            for connection in &connected {
                match detected.listing(&connection.id) {
                    Some(listing) if !listing.models.is_empty() => {
                        println!("  {} ({}):", connection.name, connection.id);
                        for model in &listing.models {
                            println!("    {}/{}", connection.id, model.id);
                        }
                    }
                    _ => println!(
                        "  {} ({}): no model list cached. Open /model in enx, or Ctrl+R there, to fetch it.",
                        connection.name, connection.id
                    ),
                }
            }
            println!("\nModel in use: {}", blank_as(&config.model.active, "none"));
            println!("Per-tier models:");
            println!(
                "  cheap:    {}",
                blank_as(
                    &config.agent.tiers.cheap,
                    "(falls back to the model in use)"
                )
            );
            println!(
                "  balanced: {}",
                blank_as(
                    &config.agent.tiers.balanced,
                    "(falls back to the model in use)"
                )
            );
            println!(
                "  strong:   {}",
                blank_as(
                    &config.agent.tiers.strong,
                    "(falls back to the model in use)"
                )
            );
            println!("Per-agent models:");
            if config.agent.models.is_empty() {
                println!("  (none set; each agent runs on its tier's model or the model in use)");
            } else {
                for (agent, model) in &config.agent.models {
                    println!("  {agent}: {model}");
                }
            }
            Ok(())
        }
        ModelsCommand::Test { model, prompt } => {
            print!("testing {model} ... ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            match enowx_core::provider::probe_model(&config, &model, &prompt).await {
                Ok(probe) => {
                    println!(
                        "ok · {} ms · in {} out {} tokens",
                        probe.elapsed.as_millis(),
                        probe.usage.input_tokens,
                        probe.usage.output_tokens
                    );
                    Ok(())
                }
                Err(error) => {
                    println!("FAILED");
                    Err(error.context(format!("{model} did not answer")))
                }
            }
        }
    }
}

fn blank_as(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}
