//! MCP servers that ship inside enx: Coolify, Dokploy, and SSH to the user's
//! VPSes. Each is served by the enx binary itself (`enx mcp serve <name>`),
//! so they need no Node or Python, and any MCP client can run them.
//!
//! A built-in server is offered to agents only once installed: `enx mcp
//! install coolify|dokploy`, or `enx vps add` for the first VPS. What is not
//! secret (URLs, hosts, users, ports) is kept in `~/.enx/builtin-mcp.json`;
//! tokens and passwords go to `auth.json` with the provider keys, readable
//! by the user alone.

pub mod coolify;
pub mod dokploy;
pub mod rag;
pub mod rag_db;
pub mod vps;

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{
    auth::Auth,
    config::{atomic_write, home_dir},
    discovery::{McpServer, McpTransport, SkillScope},
};

/// The built-in servers, by the name they are installed and served under.
pub const NAMES: [&str; 4] = ["coolify", "dokploy", "vps", "rag"];

pub fn config_path() -> PathBuf {
    home_dir().join("builtin-mcp.json")
}

/// The non-secret half of each built-in server's setup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuiltinConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coolify: Option<Endpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dokploy: Option<Endpoint>,
    /// VPSes by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vps: BTreeMap<String, vps::Host>,
    /// Retrieval over the project's code. Its database DSN and Voyage key
    /// are secrets, in `auth.json` under [`rag_dsn_id`] and [`secret_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rag: Option<rag::RagSetup>,
}

/// A panel's address. Its token is in `auth.json` under [`secret_id`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
}

/// The `auth.json` entry holding the RAG database's connection string: a
/// secret, since it carries the database password.
pub fn rag_dsn_id() -> String {
    secret_id("rag-dsn")
}

/// The `auth.json` entry holding a built-in server's token, or a VPS's
/// password.
pub fn secret_id(server: &str) -> String {
    format!("mcp-{server}")
}

impl BuiltinConfig {
    /// Read the file; a missing one is nothing installed.
    pub fn load() -> Result<Self> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, format!("{text}\n").as_bytes())?;
        Ok(path)
    }

    /// Whether `name` is set up enough to run.
    pub fn is_installed(&self, name: &str) -> bool {
        match name {
            "coolify" => self.coolify.is_some(),
            "dokploy" => self.dokploy.is_some(),
            "vps" => !self.vps.is_empty(),
            "rag" => self.rag.is_some(),
            _ => false,
        }
    }
}

/// One line describing a built-in server for the MCP menu.
pub fn summary(name: &str) -> &'static str {
    match name {
        "coolify" => "Coolify: deploy, start, stop and read your apps, databases and servers",
        "dokploy" => "Dokploy: deploy and manage your projects, apps and compose stacks",
        "vps" => "Your VPSes over SSH: run a command, or read a host's status",
        "rag" => "Search this project's code by meaning: embeddings in pgvector (Settings > RAG)",
        _ => "",
    }
}

/// All three built-in servers as MCP declarations, always listed. `enabled`
/// comes from the overlay (default off); `configured` says whether the server
/// has the credentials it needs, so the menu can refuse to turn on an empty
/// one and offer `c` to fill it in. Each is run by this binary over stdio.
pub fn builtin_servers(
    workspace: &std::path::Path,
    enabled: impl Fn(&str) -> bool,
) -> Vec<McpServer> {
    let config = BuiltinConfig::load().unwrap_or_default();
    let exe = std::env::current_exe().unwrap_or_else(|_| "enowx".into());
    NAMES
        .iter()
        .map(|name| {
            let configured = config.is_installed(name);
            McpServer {
                name: (*name).to_owned(),
                scope: SkillScope::Builtin,
                command_or_url: exe.display().to_string(),
                args: vec!["mcp".into(), "serve".into(), (*name).to_owned()],
                // The workspace the session works in, which `rag` indexes; the
                // process itself inherits wherever enx was started.
                env: BTreeMap::from([(
                    "ENX_WORKSPACE".to_owned(),
                    workspace.display().to_string(),
                )]),
                transport: McpTransport::Stdio,
                source: config_path(),
                // Off until configured and turned on.
                enabled: configured && enabled(name),
                builtin: true,
                configured,
            }
        })
        .collect()
}

/// One tool a built-in server offers.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// What a built-in server does: its tools, and running one.
#[async_trait::async_trait]
pub trait Server: Send + Sync {
    /// Work the server does by itself once it runs, beside the calls.
    fn start(self: std::sync::Arc<Self>) {}
    /// A notification from the client: no reply is sent.
    fn notified(&self, _method: &str, _params: &Value) {}
    fn tools(&self) -> Vec<ToolSpec>;
    /// The tool's text result. An error is reported to the model as a
    /// failed call, not as a protocol error.
    async fn call(&self, tool: &str, args: &Value) -> Result<String>;
}

/// The server `name` with its stored setup.
pub fn server(name: &str) -> Result<Box<dyn Server>> {
    let config = BuiltinConfig::load()?;
    let auth = Auth::load()?;
    let token = |server: &str| {
        auth.key(&secret_id(server), &[])
            .map(|(key, _)| key)
            .with_context(|| format!("no {server} token stored; press c on {server} in /mcp, or run `enx mcp set {server}`"))
    };
    Ok(match name {
        "coolify" => {
            let endpoint = config
                .coolify
                .context("coolify is not installed; run `enx mcp install coolify`")?;
            Box::new(coolify::Coolify::new(
                &endpoint.base_url,
                &token("coolify")?,
            )?)
        }
        "dokploy" => {
            let endpoint = config
                .dokploy
                .context("dokploy is not installed; run `enx mcp install dokploy`")?;
            Box::new(dokploy::Dokploy::new(
                &endpoint.base_url,
                &token("dokploy")?,
            )?)
        }
        "vps" => {
            if config.vps.is_empty() {
                bail!("no VPS is set up; add one with `enx vps add`");
            }
            Box::new(vps::Vps::new(config.vps, auth))
        }
        "rag" => {
            let setup = config
                .rag
                .context("rag is not set up; open Settings > RAG, or run `enx mcp set rag`")?;
            let stored_dsn = auth.key(&rag_dsn_id(), &[]).map(|(key, _)| key);
            let backend = setup.effective_database_backend(stored_dsn.is_some())?;
            let dsn = match backend {
                rag::DatabaseBackend::Embedded => None,
                rag::DatabaseBackend::Postgres => {
                    Some(stored_dsn.context("no database stored for rag; open Settings > RAG")?)
                }
            };
            // A local endpoint (Ollama, LM Studio) needs no key.
            let key = match setup.provider() {
                rag::Provider::Custom => auth
                    .key(&secret_id("rag"), &[])
                    .map(|(key, _)| key.to_owned())
                    .unwrap_or_default(),
                _ => token("rag")?,
            };
            Box::new(rag::Rag::new(backend, dsn.as_deref(), &key, &setup)?)
        }
        other => bail!(
            "enx has no built-in MCP server `{other}` (built in: {})",
            NAMES.join(", ")
        ),
    })
}

/// Serve `name` over stdio until the client hangs up: JSON-RPC 2.0, one
/// message per line, as MCP clients speak it.
pub async fn serve(name: &str) -> Result<()> {
    let server: std::sync::Arc<dyn Server> = std::sync::Arc::from(server(name)?);
    server.clone().start();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // A notification (no id) gets no reply; the server may act on it.
        if message.get("id").is_none() {
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                server.notified(method, &params);
            }
            continue;
        }
        if let Some(reply) = answer(name, server.as_ref(), &message).await {
            stdout.write_all(format!("{reply}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// The reply to one message, or `None` for a notification.
async fn answer(name: &str, server: &dyn Server, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": format!("enx-{name}"), "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": server.tools().into_iter().map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            })).collect::<Vec<_>>(),
        }),
        "tools/call" => {
            let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let (text, failed) = match server.call(tool, &args).await {
                Ok(text) => (text, false),
                Err(error) => (format!("{error:#}"), true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": failed })
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }))
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// A required string argument.
pub(crate) fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("`{key}` is required"))
}

/// A JSON schema for an object of string properties, `required` among them.
pub(crate) fn schema(properties: &[(&str, &str)], required: &[&str]) -> Value {
    let props: serde_json::Map<String, Value> = properties
        .iter()
        .map(|(name, description)| {
            (
                (*name).to_owned(),
                json!({ "type": "string", "description": description }),
            )
        })
        .collect();
    json!({ "type": "object", "properties": props, "required": required })
}

/// Field names whose values are never shown to the model.
pub(crate) fn is_secret_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "private_key",
        "api_key",
        "apikey",
        "passphrase",
    ]
    .iter()
    .any(|word| name.contains(word))
}

/// `value` with every secret-looking field's value replaced, at any depth.
pub(crate) fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if is_secret_field(key) && !inner.is_null() {
                    *inner = Value::String("[redacted]".into());
                } else {
                    redact(inner);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// `value` as indented JSON, cut to a size a model can read.
pub(crate) fn render(value: &Value) -> String {
    const LIMIT: usize = 60_000;
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    if text.len() <= LIMIT {
        return text;
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… (cut at {LIMIT} of {} bytes)",
        &text[..end],
        text.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl Server for Echo {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "echo",
                description: "says it back",
                input_schema: schema(&[("text", "what to say")], &["text"]),
            }]
        }
        async fn call(&self, _tool: &str, args: &Value) -> Result<String> {
            Ok(arg(args, "text")?.to_owned())
        }
    }

    #[tokio::test]
    async fn it_speaks_mcp() {
        let init = answer("t", &Echo, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).await.unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert!(answer(
            "t",
            &Echo,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .await
        .is_none());
        let list = answer(
            "t",
            &Echo,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await
        .unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "echo");
        let call = answer("t", &Echo, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}})).await.unwrap();
        assert_eq!(call["result"]["content"][0]["text"], "hi");
        assert_eq!(call["result"]["isError"], false);
        let failed = answer("t", &Echo, &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo","arguments":{}}})).await.unwrap();
        assert_eq!(failed["result"]["isError"], true);
        let unknown = answer(
            "t",
            &Echo,
            &json!({"jsonrpc":"2.0","id":5,"method":"resources/list"}),
        )
        .await
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn all_three_are_listed_off_until_configured() {
        // With no config file, every built-in server is listed, off, and not
        // configured (so the menu offers `c`).
        let servers = builtin_servers(std::path::Path::new("/w"), |_| true);
        assert_eq!(servers.len(), NAMES.len());
        for server in &servers {
            assert!(server.builtin);
            assert!(!server.configured, "{} needs setup first", server.name);
            assert!(!server.enabled, "{} is off until configured", server.name);
        }
    }

    #[test]
    fn secrets_are_never_shown() {
        let mut value = json!({"name":"app","env":[{"key":"DB_PASSWORD","value":"x"}],"api_token":"t","nested":{"private_key":"k","port":5432}});
        redact(&mut value);
        assert_eq!(value["api_token"], "[redacted]");
        assert_eq!(value["nested"]["private_key"], "[redacted]");
        assert_eq!(value["nested"]["port"], 5432);
        assert_eq!(value["name"], "app");
    }
}
