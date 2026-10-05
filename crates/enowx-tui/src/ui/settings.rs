use super::*;

pub(super) fn draw_settings(frame: &mut Frame, app: &mut App, area: Rect) {
    let fields = app.current_form_fields();
    let width = area.width.saturating_sub(2).min(72);
    // Content rows: each field is its label, its value and a gap; the note
    // sits on the last row with a blank row above it.
    let height = (fields.len() as u16 * 3).max(3) + 1;
    // Keys go in the box's bottom edge; the sentence explaining the form, or
    // the error when there is one, keeps one row inside.
    let provider = app.settings.name.as_str();
    let key_url = enowx_core::provider_preset(&app.settings.provider_id)
        .map(|preset| preset.key_url)
        .filter(|url| !url.is_empty());
    let (keys, note, title): (&str, String, String) = match app.modal {
        Modal::TypeSafeKey => (
            "Enter save · Ctrl+U clear · Esc cancel",
            "An empty key turns TypeSafe off. TYPESAFE_API_KEY is read too.".into(),
            "TYPESAFE KEY".into(),
        ),
        Modal::ProviderKey => (
            "Enter connect · Ctrl+U clear · Esc back",
            match key_url {
                Some(url) => format!("Kept in ~/.enx/auth.json. Create one at {url}"),
                None => "Kept in ~/.enx/auth.json, beside your other providers' keys.".into(),
            },
            format!("{} KEY", provider.to_uppercase()),
        ),
        Modal::ModelManual => (
            "Tab field · Enter add and use · Esc back",
            "For a model its list lacks. An empty window uses 128k.".into(),
            format!("ADD A MODEL TO {}", provider.to_uppercase()),
        ),
        Modal::ModelEdit => (
            "Tab field · ←→ choose · Enter save · Esc back",
            "Overrides the detected values. Empty uses the catalogue or the id.".into(),
            format!("EDIT {}", app.settings.model.to_uppercase()),
        ),
        Modal::General => (
            "Tab field · ←→ choose · Enter save · Esc back",
            "Saved to ~/.enx/config.toml; used from the next turn.".into(),
            "GENERAL · HOW THE AGENTS WORK".into(),
        ),
        Modal::Display => (
            "Tab field · ←→ choose · Enter save · Esc back",
            "Saved to ~/.enx/config.toml.".into(),
            "DISPLAY".into(),
        ),
        Modal::Updates => (
            "Tab field · ←→ choose · Enter save · Esc back",
            "/update installs the latest now; `enx update` does it from a shell.".into(),
            format!("UPDATES · ENOWX {}", enowx_core::update::current()),
        ),
        Modal::Team => (
            "Tab field · ←→ choose · Enter save · Esc back",
            "Off by default. Saved to ~/.enx/config.toml; applies from the next step.".into(),
            "TEAM · AGENTS WORKING TOGETHER".into(),
        ),
        Modal::Rag => (
            "Tab field · ←→ choose · Enter save · Esc chat",
            "Keys and the database go to ~/.enx/auth.json; blank keeps what is stored.".into(),
            "RAG · CODE SEARCH".into(),
        ),
        Modal::BuiltinMcp => (
            "Tab field · Enter save and turn on · Esc back",
            "Kept in ~/.enx/. The token or password goes to auth.json, for you alone.".into(),
            format!("SET UP {}", app.settings.provider_id.to_uppercase()),
        ),
        _ => (
            if width < 44 {
                "Tab field · Enter save · Esc back"
            } else {
                "Tab / ↑↓ field · Ctrl+U clear · Enter save · Esc back"
            },
            if app.settings.provider_id.is_empty() {
                "Any OpenAI-compatible endpoint. The key goes to ~/.enx/auth.json.".into()
            } else {
                "An empty key keeps the stored one.".into()
            },
            provider_form_title(app),
        ),
    };
    let (_, content) = overlay(frame, app, width, height, &title, keys);
    if content.height == 0 || content.width < 4 {
        return;
    }
    let note_row = Rect::new(content.x, content.bottom() - 1, content.width, 1);
    let area = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(2),
    );
    // The last field needs no gap below it.
    let visible = ((area.height + 1) / 3).max(1) as usize;
    let (start, _) = list_window(app, &vec![3; fields.len()], area.height + 1);
    for (index, field) in fields.iter().copied().enumerate().skip(start).take(visible) {
        let active = index == app.modal_cursor;
        let raw = app.settings.value(field);
        let choice = field.is_choice();
        let shown = if field.is_secret() {
            "•".repeat(raw.chars().count())
        } else if choice {
            format!("◂ {} ▸", app.settings.choice_shown(field))
        } else {
            raw.to_owned()
        };
        let y = area.y + ((index - start) * 3) as u16;
        // Label and value both have to fit: a label with its value cut off
        // below the box reads as an empty field.
        if y + 1 >= area.bottom() {
            break;
        }
        // Marker on the overlay's column 2, the label and its value both on
        // column 4, so a field reads as one aligned block.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if active { "› " } else { "  " },
                    Style::default().fg(app.theme.accent),
                ),
                Span::styled(
                    field.label(),
                    if active {
                        Style::default()
                            .fg(app.theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(app.theme.muted)
                    },
                ),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
        let field_area = Rect::new(area.x + 2, y + 1, area.width.saturating_sub(2), 1);
        let cursor_column = if !active || choice {
            0
        } else if field.is_secret() {
            raw[..app.field_cursor].chars().count()
        } else {
            raw[..app.field_cursor].width()
        };
        let offset = if active {
            cursor_column.saturating_sub(field_area.width.saturating_sub(1) as usize)
        } else {
            0
        };
        let placeholder = placeholder(app, field);
        let content_text = if shown.is_empty() {
            placeholder
        } else {
            &shown
        };
        frame.render_widget(
            Paragraph::new(content_text)
                .scroll((0, offset.min(u16::MAX as usize) as u16))
                .style(Style::default().fg(if shown.is_empty() {
                    app.theme.faint
                } else {
                    app.theme.text
                })),
            field_area,
        );
        // No caret in a form while the Settings section list has the keys.
        if active && field_area.width > 0 && !app.settings_nav {
            frame.set_cursor_position((
                field_area.x + (cursor_column - offset) as u16,
                field_area.y,
            ));
        }
    }
    let note = note.as_str();
    let (note, colour) = if app.modal_error.is_empty() {
        (note, app.theme.faint)
    } else {
        (app.modal_error.as_str(), app.theme.red)
    };
    frame.render_widget(
        Paragraph::new(trim(note, note_row.width as usize)).style(Style::default().fg(colour)),
        note_row,
    );
}

fn provider_form_title(app: &App) -> String {
    if app.settings.provider_id.is_empty() {
        "CUSTOM PROVIDER".to_owned()
    } else {
        app.settings.name.to_uppercase()
    }
}

/// What an empty field shows.
fn placeholder(app: &App, field: SettingsField) -> &'static str {
    let stored = !app.settings.provider_id.is_empty()
        && app
            .config
            .connection(&app.settings.provider_id)
            .is_some_and(|c| c.key.is_some());
    match field {
        SettingsField::Name => "e.g. My gateway",
        SettingsField::BaseUrl => "https://host/v1",
        SettingsField::ApiKey if app.modal == Modal::Rag => {
            use enowx_core::builtin_mcp::rag::Provider;
            match Provider::parse(&app.settings.rag_provider) {
                Provider::Voyage => "Voyage AI key (blank keeps the stored one)",
                Provider::OpenAi => "OpenAI key (blank keeps the stored one)",
                Provider::Custom => "optional for a local endpoint",
            }
        }
        SettingsField::Dsn if app.modal == Modal::Rag => "Postgres only; blank retains a saved DSN",
        SettingsField::DatabaseBackend => "←→ Embedded / Postgres",
        SettingsField::ApiKey if stored => "(stored; type to replace it)",
        SettingsField::ApiKey => "(empty)",
        SettingsField::ModelsUrl => "https://host/v1/models",
        SettingsField::Model => "the id the provider uses",
        SettingsField::ContextWindow => "e.g. 200000",
        SettingsField::Effort | SettingsField::Vision => "←→ to choose",
        SettingsField::PriceInput | SettingsField::PriceOutput => "e.g. 3.0 (blank = unknown)",
        SettingsField::Host => "203.0.113.5, a hostname, or a ~/.ssh/config alias",
        SettingsField::User => "root (blank: from ~/.ssh/config)",
        SettingsField::Port => "22",
        SettingsField::Dsn => {
            "postgres://localhost/enx (local) or a cloud URL with ?sslmode=require"
        }
        SettingsField::EmbedUrl => "http://localhost:11434/v1 (Ollama), or any /v1 base",
        SettingsField::EmbedModelText => "e.g. nomic-embed-text, jina-embeddings-v3",
        SettingsField::DimensionText => "the width the model returns, e.g. 768",
        SettingsField::RerankText => "blank: none (needs a /rerank endpoint)",
        SettingsField::TeamEnabled
        | SettingsField::TeamMessages
        | SettingsField::TeamBoard
        | SettingsField::TeamReview
        | SettingsField::ReviewRounds
        | SettingsField::Reviewer
        | SettingsField::UpdateCheck
        | SettingsField::UpdateAuto => "←→ to choose",
        SettingsField::RagEnabled
        | SettingsField::AutoIndex
        | SettingsField::RagProvider
        | SettingsField::EmbedModel
        | SettingsField::Dimension
        | SettingsField::Rerank => "←→ to choose",
        SettingsField::Conf(i) => match crate::modal::CONF_FIELDS.get(i) {
            Some((_, _, crate::modal::ConfKind::Text, hint)) => hint,
            _ => "←→ to choose",
        },
        SettingsField::KeyFile => "~/.ssh/id_ed25519 (blank: ssh-agent, ~/.ssh keys)",
        SettingsField::Passphrase | SettingsField::Password => "(blank keeps what is stored)",
    }
}
