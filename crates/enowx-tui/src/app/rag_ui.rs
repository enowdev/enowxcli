//! Settings > RAG: code search on or off, the Postgres it keeps its index
//! in, and where its embeddings come from (Voyage AI, OpenAI, or any
//! OpenAI-compatible endpoint), with the model, the vector width and the
//! reranker. Saving reloads MCP in place, as `c` on a built-in server does.

use super::*;
use enowx_core::builtin_mcp::{rag, rag_dsn_id, secret_id, BuiltinConfig};

impl App {
    /// Whether the rag server is on, as `/mcp` shows it.
    fn rag_on(&self) -> bool {
        self.discovery
            .mcp_servers
            .iter()
            .any(|s| s.builtin && s.name == "rag" && s.enabled)
    }

    /// Open the RAG section, filled with what is set. Secrets are never
    /// shown: a blank field keeps the stored one.
    pub(crate) fn open_rag(&mut self) {
        let setup = BuiltinConfig::load()
            .unwrap_or_default()
            .rag
            .unwrap_or_default();
        let provider = setup.provider();
        let auth = enowx_core::auth::Auth::load().unwrap_or_default();
        let has_dsn = auth.key(&rag_dsn_id(), &[]).is_some();
        let backend = setup
            .effective_database_backend(has_dsn)
            .unwrap_or_else(|_| rag::DatabaseBackend::Postgres);
        self.settings = crate::modal::SettingsDraft {
            provider_id: "rag".into(),
            name: "rag".into(),
            rag_enabled: if self.rag_on() { "on" } else { "off" }.into(),
            database_backend: match backend {
                rag::DatabaseBackend::Embedded => "embedded",
                rag::DatabaseBackend::Postgres => "postgres",
            }
            .into(),
            rag_provider: provider.id().into(),
            base_url: setup.base_url.clone(),
            model: setup.model.clone(),
            dimension: if setup.dimension > 0 {
                setup.dimension.to_string()
            } else {
                String::new()
            },
            rerank: setup.rerank.clone(),
            auto_index: if setup.auto_index() { "on" } else { "off" }.into(),
            ..Default::default()
        };
        self.modal_cursor = 0;
        self.field_cursor = 0;
        self.modal_error.clear();
        self.modal = Modal::Rag;
    }

    /// Enter in the RAG section: store what changed, then turn the server on
    /// or off and reload MCP. The section stays open.
    pub(crate) fn save_rag(&mut self) -> Result<()> {
        let setup = self.settings.rag_setup();
        let provider = setup.provider();
        let on = self.settings.rag_enabled == "on";
        let mut auth = enowx_core::auth::Auth::load()?;
        let dsn = self.settings.dsn.trim().to_owned();
        let key = self.settings.api_key.trim().to_owned();
        let embedded = setup.database == Some(rag::DatabaseBackend::Embedded);
        if !embedded
            && !dsn.is_empty()
            && !dsn.starts_with("postgres://")
            && !dsn.starts_with("postgresql://")
        {
            self.modal_error = "the database must be a postgres:// connection string".into();
            return Ok(());
        }
        if let Err(error) = setup.check() {
            self.modal_error = format!("{error:#}");
            return Ok(());
        }
        let has_dsn = !dsn.is_empty() || auth.key(&rag_dsn_id(), &[]).is_some();
        let has_key = !key.is_empty() || auth.key(&secret_id("rag"), &[]).is_some();
        if embedded && !rag::DatabaseBackend::embedded_supported() {
            self.modal_error = "embedded RAG is not supported on this target".into();
            return Ok(());
        }
        if on && !embedded && !has_dsn {
            self.modal_error = "a database is needed to turn RAG on with Postgres".into();
            return Ok(());
        }
        if on && !has_key && provider != rag::Provider::Custom {
            self.modal_error = format!(
                "an API key for {} is needed to turn RAG on",
                provider.label()
            );
            return Ok(());
        }
        if !embedded && !dsn.is_empty() {
            auth.store(&rag_dsn_id(), &dsn)?;
        }
        if !key.is_empty() {
            auth.store(&secret_id("rag"), &key)?;
        }
        let mut config = BuiltinConfig::load().unwrap_or_default();
        let changed_model = config
            .rag
            .as_ref()
            .is_some_and(|old| old.embedder() != setup.embedder());
        config.rag = Some(setup.clone());
        config.save()?;
        enowx_core::persist::set_mcp_enabled("rag", on)?;
        self.adopt(self.config.clone());
        self.settings.dsn.clear();
        self.settings.api_key.clear();
        self.field_cursor = 0;
        self.modal = Modal::Rag;
        self.status = match (on, changed_model) {
            (true, true) => format!(
                "RAG on with {}; the next index embeds the project again",
                setup.model()
            ),
            (true, false) => format!("RAG on with {}", setup.model()),
            (false, _) => "RAG off".into(),
        };
        Ok(())
    }
}
