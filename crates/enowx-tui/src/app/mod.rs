use crate::{
    commands::COMMANDS,
    modal::{Modal, SettingsDraft, SettingsField},
    session::{Activity, TranscriptBlock, TranscriptKind, SPINNER},
    theme::{Theme, THEMES},
};
use anyhow::Result;
mod attach;
use enowx_core::{
    discovery::Discovery,
    provider::{ModelInfo, Provider},
    Agent, Config, Event, MessageRole, Role, RunRequest, SessionStore,
};
use ratatui::layout::Rect;
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
mod actions;
mod connection;
pub(crate) mod delegation_list;
mod events;
mod handoff;
mod keys;
pub(crate) mod mcp_ui;
pub(crate) mod model_picker;
mod navigation;
pub(crate) mod pages;
mod prefs_ui;
pub(crate) mod question;
mod queue;
mod rag_ui;
mod sessions;
mod settings_keys;
pub(crate) mod skills;
mod team_ui;
pub(crate) mod update_ui;

/// What one delegated agent has used so far.
#[derive(Clone, Debug, Default)]
pub(crate) struct BranchUsage {
    pub(crate) agent: String,
    pub(crate) input: u32,
    pub(crate) output: u32,
    pub(crate) context: u32,
    pub(crate) window: u32,
    /// It reported: its tokens are about to be the delegating session's.
    pub(crate) done: bool,
}

impl App {
    /// The context the SESSION card shows: the sub-agent's whose transcript
    /// is open, or else the conversation's. (used, window, whose).
    pub(crate) fn shown_context(&self) -> (u32, u32, Option<String>) {
        if let Some(viewing) = &self.viewing {
            let usage = self.branch_usage.get(&viewing.session_id);
            return (
                usage.map_or(0, |u| u.context),
                usage
                    .map(|u| u.window)
                    .filter(|w| *w > 0)
                    .unwrap_or(self.context_window),
                Some(viewing.agent.clone()),
            );
        }
        (self.context_tokens, self.context_window, None)
    }

    /// Tokens the whole session used so far, in and out: the conversation
    /// with what its finished delegations spent, and what those at work
    /// have spent until they report.
    pub(crate) fn session_tokens(&self) -> (u64, u64) {
        self.branch_usage.values().fold(
            (self.tokens_in as u64, self.tokens_out as u64),
            |(input, output), u| (input + u.input as u64, output + u.output as u64),
        )
    }
}

/// One sub-agent run, as the sidebar shows it.
#[derive(Clone)]
pub(crate) struct Delegation {
    pub(crate) agent: String,
    pub(crate) task: String,
    pub(crate) session_id: String,
    pub(crate) state: DelegationState,
    /// The model this sub-agent runs on, as resolved when it started.
    pub(crate) model: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DelegationState {
    Running,
    Finished,
    Failed,
}

/// How many delegations in `blocks` have reported back.
pub(crate) fn reports_in(blocks: &[TranscriptBlock]) -> usize {
    blocks
        .iter()
        .filter(|block| {
            matches!(&block.kind, TranscriptKind::Brief { report, .. } if !report.is_empty())
        })
        .count()
}

impl DelegationState {
    pub(crate) fn marker(self) -> &'static str {
        match self {
            DelegationState::Running => "◆",
            DelegationState::Finished => "✓",
            DelegationState::Failed => "✗",
        }
    }
}

/// What the main conversation looked like before a branch was opened, so
/// going back restores it rather than reloading and losing the scroll.
pub(crate) struct Viewing {
    pub(crate) blocks: Vec<crate::session::TranscriptBlock>,
    pub(crate) scroll: u16,
    pub(crate) auto_scroll: bool,
    /// The agent whose branch is on screen.
    pub(crate) agent: String,
    /// Which delegation is being viewed, so its state can be read back.
    pub(crate) index: usize,
    /// Branch session on disk, re-read while the sub-agent is still writing
    /// to it.
    pub(crate) session_id: String,
    /// When it was last re-read. A running branch grows, and a transcript
    /// frozen at the moment it was opened is the thing that makes a working
    /// sub-agent look stopped.
    pub(crate) last_refresh: Instant,
    /// How many blocks the conversation had when the branch was opened, so
    /// the footer can say it has moved on without the user having to leave
    /// to find out.
    pub(crate) blocks_at_open: usize,
    /// Delegation reports in it then; see `reports_in`.
    pub(crate) reports_at_open: usize,
}

pub(crate) struct App {
    pub(crate) agent: Arc<Agent>,
    /// What delegations still running after their turn send: one receiver
    /// per agent this app has had, since an agent replaced by a settings
    /// change keeps its delegations running to the end.
    pub(crate) background: Vec<mpsc::Receiver<Event>>,
    /// Reports of background delegations waiting to wake the orchestrator,
    /// by the session that delegated.
    pub(crate) reports: Vec<(String, Vec<enowx_core::event::DelegationReport>)>,
    /// The catalogue fetch this app last looked its model up in.
    pub(crate) catalog_seen: u64,
    pub(crate) config: Config,
    pub(crate) store: SessionStore,
    pub(crate) blocks: Vec<TranscriptBlock>,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) session_id: Option<String>,
    pub(crate) title: String,
    pub(crate) role: Role,
    /// Which agent currently holds the session. Empty means "whatever the
    /// legacy role maps to" — the loop has not been migrated yet, so this is
    /// resolved through `App::active_agent` rather than being assumed set.
    pub(crate) agent_name: String,
    /// Handovers to mark in the transcript, each pinned to the block it
    /// precedes rather than to `AgentSwitch::at_turn`. A turn expands into
    /// several blocks — reasoning, prose, one per tool call — so a turn index
    /// alone cannot say where the rule belongs once the transcript is built.
    pub(crate) switch_markers: Vec<(usize, enowx_core::session::AgentSwitch)>,
    pub(crate) busy: bool,
    /// How many stopped turns still owe a `Done`. A count rather than a flag:
    /// a flag set by one stop and cleared by the next `Done` swallowed the
    /// wrong turn's completion when the stopped turn never sent one.
    pub(crate) abandoned: usize,
    /// How many tool results TypeSafe has trimmed this session, and how many
    /// characters that saved. Shown in the sidebar so a feature that removes
    /// text from the model's context can be seen doing it.
    /// Delegations this session has started, newest last. The sidebar lists
    /// them and a click opens the branch they ran in.
    pub(crate) delegations: Vec<Delegation>,
    /// Set while viewing a branch: what to restore on the way back.
    pub(crate) viewing: Option<Viewing>,
    /// When the home screen went up, for its opening animation. Cleared when
    /// the chat layout takes over, so the next home screen plays it again.
    pub(crate) home_started: Option<std::time::Instant>,
    /// On-screen rows of the delegation list, so a click finds which one was
    /// hit. Rebuilt each frame from the line indices the sidebar returns.
    pub(crate) delegation_rects: Vec<(Rect, usize)>,
    /// The first delegation the sidebar lists when it was slid back to
    /// older ones; `None` follows the newest.
    pub(crate) delegation_window: Option<usize>,
    /// Where the delegation list is on screen, for the wheel to slide it.
    pub(crate) delegation_list_area: Option<Rect>,
    /// The "earlier" and "more" rows, and how far a click on each slides.
    pub(crate) delegation_slide_rects: Vec<(Rect, isize)>,
    /// What has happened this session, for the LOGS tab.
    pub(crate) logs: crate::logs::Logs,
    /// Which kind the LOGS tab is showing; `None` is everything.
    pub(crate) log_filter: usize,
    /// Whether the technical line under each entry is shown.
    pub(crate) log_detail: bool,
    /// When the session started, so log lines can be stamped against it.
    pub(crate) started: Instant,
    pub(crate) trimmed_count: usize,
    pub(crate) trimmed_saved: usize,
    /// Result of the last TypeSafe key check, awaited off the UI thread.
    pub(crate) typesafe_check: Option<mpsc::Receiver<Result<(), String>>>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) events: Option<mpsc::Receiver<Event>>,
    pub(crate) task: Option<tokio::task::JoinHandle<()>>,
    /// Whether thinking opens by default; each row still toggles on a click.
    pub(crate) show_reasoning: bool,
    /// Numbers thinking blocks so each has an id to open by.
    pub(crate) reasoning_seq: usize,
    pub(crate) show_tool_output: bool,
    /// Per-block override of `show_tool_output`. Keyed by the tool call id so
    /// re-renders keep the same open/closed state after a scroll or resize.
    pub(crate) tool_expanded: std::collections::HashMap<String, bool>,
    /// What a file held before a `write` replaced it, by call id, so the row
    /// can show the change as a diff. Not kept in the session file: a resumed
    /// write shows the new content instead.
    pub(crate) tool_before: std::collections::HashMap<String, String>,
    /// (tool_id, line_index_in_wrapped_transcript) captured by the renderer
    /// each frame so a click on the header line can toggle expansion.
    pub(crate) tool_header_markers: Vec<(String, usize)>,
    /// Screen rects (already scroll-adjusted) of tool header lines this frame,
    /// so a mouse click can find which tool block to toggle.
    pub(crate) tool_header_rects: Vec<(Rect, String)>,
    /// Transcript body area, used to route wheel events to scroll only when
    /// the pointer is inside it.
    pub(crate) transcript_area: Option<Rect>,
    /// Composer field rect + viewport offset in visual rows, so a mouse
    /// click on the field can be translated back into a byte offset in
    /// `input`.
    pub(crate) composer_field: Option<Rect>,
    pub(crate) composer_offset: usize,
    pub(crate) composer_width: usize,
    /// Mouse drag selection over the transcript. Filled on Down(Left) inside
    /// the transcript area, extended on Drag, extracted+cleared on Up.
    pub(crate) selection: Option<TextSelection>,
    /// Snapshot of every wrapped visual line rendered this frame with the
    /// screen row it occupies. Populated by `draw_transcript` so a drag
    /// selection can extract exactly what the user saw.
    pub(crate) wrapped_snapshot: Vec<(u16, String)>,
    pub(crate) scroll: u16,
    pub(crate) max_scroll: u16,
    pub(crate) auto_scroll: bool,
    pub(crate) should_quit: bool,
    pub(crate) status: String,
    pub(crate) tokens_in: u32,
    pub(crate) tokens_out: u32,
    pub(crate) modal: Modal,
    pub(crate) modal_cursor: usize,
    pub(crate) modal_items: Vec<(String, String)>,
    pub(crate) palette_cursor: usize,
    /// First line of the Ctrl+P palette on screen, kept between frames so
    /// the list scrolls rather than jumping to put the selection at an edge.
    pub(crate) palette_offset: usize,
    /// The first row a modal list shows. It follows the selection while
    /// the keys move it; the wheel moves it on its own (`modal_scrolled`),
    /// one row at a time, and leaves the selection where it was.
    pub(crate) modal_offset: usize,
    pub(crate) modal_scrolled: bool,
    /// The modal `modal_offset` belongs to: another one starts at its top.
    pub(crate) modal_offset_for: Modal,
    /// The update check and install, shared with the task doing them.
    pub(crate) update: std::sync::Arc<std::sync::Mutex<update_ui::UpdateState>>,
    /// The update state last said on screen.
    pub(crate) update_shown: update_ui::UpdateState,
    /// Whether the user asked (`/update`), and so hears every outcome.
    pub(crate) update_heard: bool,
    /// What each delegated agent has used, by its session, from its own
    /// `Usage` events. Counted into the session's totals until the session
    /// that delegated has taken it into its own.
    pub(crate) branch_usage: std::collections::HashMap<String, BranchUsage>,
    /// The inline command list above the composer, and its rows, for the
    /// wheel and for clicks.
    pub(crate) composer_palette: Option<Rect>,
    pub(crate) composer_palette_rows: Vec<(Rect, usize)>,
    /// A question the agent holding the conversation is waiting on.
    pub(crate) question: Option<question::PendingQuestion>,
    /// On-screen rows of the question's options, so a click answers.
    pub(crate) question_rows: Vec<(Rect, usize)>,
    /// The last answer sent, for tests: the agent it went to keeps no
    /// record a test can read.
    pub(crate) last_answer: Option<enowx_core::ask::Answer>,
    pub(crate) settings: SettingsDraft,
    pub(crate) field_cursor: usize,
    /// The `/model` list: each connected provider's models, and the
    /// recent and favourite picks.
    pub(crate) picker: model_picker::ModelPicker,
    /// The agent the model list is choosing for (`m` in `/agent`), rather
    /// than the conversation's model.
    pub(crate) picking_for_agent: Option<String>,
    /// The provider id on each row of `/provider`, in order; the row after
    /// them adds a new one.
    pub(crate) provider_ids: Vec<String>,
    pub(crate) modal_error: String,
    pub(crate) activity: Activity,
    pub(crate) activity_since: Instant,
    pub(crate) turn_started: Instant,
    pub(crate) theme: Theme,
    pub(crate) sidebar_tab: usize,
    /// The first line of the side card on screen; it scrolls a line a
    /// wheel step, or a card's height with Alt+arrows and the pager.
    pub(crate) sidebar_page: usize,
    /// The positions it can scroll to: the lines past a full card, plus one.
    pub(crate) sidebar_pages: usize,
    /// How many lines the card shows.
    pub(crate) sidebar_view: usize,
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) show_sidebar: bool,
    pub(crate) attachments: Vec<enowx_core::message::Attachment>,
    /// Messages sent while a turn ran, waiting their turn, first first.
    pub(crate) queued: std::collections::VecDeque<queue::Queued>,
    /// Set when the user stopped a turn: the queue waits for Ctrl+Enter rather
    /// than sending the next message the moment the turn is gone.
    pub(crate) queue_paused: bool,
    /// Where the queue's "send now" button was drawn, for a click.
    pub(crate) queue_send_button: Option<Rect>,
    /// The main column on the last frame: where a settings page is drawn.
    pub(crate) main_area: Option<Rect>,
    /// The tabs at the top right, as drawn, for a click.
    pub(crate) page_tabs: Vec<(Rect, pages::Tab)>,
    /// The tab last chosen, by its place in `pages::PAGES`.
    pub(crate) page_index: usize,
    /// Settings has the section list focused rather than the section.
    pub(crate) settings_nav: bool,
    /// Where each section of the Settings list was drawn, for clicks.
    pub(crate) settings_sections: Vec<(Rect, pages::Page)>,
    /// The area a Settings section draws in, beside the section list.
    pub(crate) settings_content: Option<Rect>,
    /// What this session costs the machine, sampled every few seconds.
    pub(crate) resources: crate::resources::Sampler,
    /// A handoff under way: the new session, or why it failed.
    pub(crate) handoff: Option<handoff::Pending>,
    /// The last-seen change time of the files that configure MCP, so a fill
    /// from the CLI (an agent running `enx mcp set ...`) reloads MCP in place
    /// without a restart.
    pub(crate) mcp_config_stamp: Option<std::time::SystemTime>,
    /// When the MCP config files were last checked for a change.
    pub(crate) mcp_reload_checked: std::time::Instant,
    pub(crate) attach_error: Option<String>,
    pub(crate) tool_counts: HashMap<String, usize>,
    pub(crate) context_tokens: u32,
    pub(crate) context_window: u32,
    pub(crate) sidebar_area: Option<Rect>,
    pub(crate) sidebar_tabs: Vec<(Rect, usize)>,
    pub(crate) modal_rows: Vec<(Rect, usize)>,
    pub(crate) sidebar_pages_area: Option<Rect>,
    pub(crate) discovery: Arc<Discovery>,
    /// Search query typed in `/skills` and `/mcp` popups.
    pub(crate) modal_search: String,
    /// Draft for the Add-MCP form popup.
    pub(crate) mcp_draft: crate::modal::McpDraft,
    pub(crate) mcp_field: usize,
    /// Popup click regions: `(row_rect, mark_rect, row_index)`. Filled by the
    /// popup renderer each frame so a click can hit either the toggle mark
    /// (Tab equivalent) or the row body (Enter equivalent).
    pub(crate) popup_rows: Vec<(Rect, Rect, usize)>,
    /// Body rect of the currently open popup, used to route ScrollUp/Down
    /// events to the popup cursor instead of the transcript.
    pub(crate) popup_body: Option<Rect>,
    /// Rects for the MCP form fields, so the user can click a row to focus it.
    pub(crate) mcp_field_rows: Vec<(Rect, usize)>,
    /// Timestamp of the last accepted wheel step. Terminals emit wheel events
    /// at burst rates (dozens per second on macOS trackpads); we throttle so
    /// one physical scroll = one selector step.
    pub(crate) last_wheel: Option<Instant>,
    /// Selection state for the QuitConfirm and StopConfirm popups. `true` =
    /// the action highlighted, `false` = cancel. Each popup opens on `false`
    /// so an accidental Enter neither quits nor stops anything.
    pub(crate) quit_confirm_yes: bool,
    /// Screen rects of the two confirm buttons this frame, so a click can
    /// trigger the matching action without keyboard.
    pub(crate) quit_confirm_rects: [(Rect, bool); 2],
    /// The sub-agent StopConfirm asks about: its branch session and agent.
    pub(crate) stop_target: Option<(String, String)>,
    /// (rect, path) markers for file paths in the transcript. Populated by
    /// tool renderers each frame; a click on `rect` opens `path` with the
    /// OS default app.
    /// (transcript row, block index) for each user message, so a click can
    /// find which one was hit. Filled while the transcript renders.
    pub(crate) user_block_markers: Vec<(usize, usize)>,
    /// The same, resolved to on-screen rects for the rows actually visible.
    pub(crate) user_block_rects: Vec<(Rect, usize)>,
    /// The user message a message menu is acting on: its index in `blocks`.
    pub(crate) message_target: Option<usize>,
    /// Draft text while a message is being edited.
    pub(crate) message_draft: String,
    pub(crate) message_draft_cursor: usize,
    pub(crate) file_link_markers: Vec<crate::ui::FileLink>,
    pub(crate) file_link_rects: Vec<(Rect, String)>,
    /// Rendered lines per transcript block, so a frame only re-parses the
    /// blocks that actually changed. Without it every keystroke and every
    /// streamed token re-ran the markdown parser over the whole session, so
    /// the cost of drawing a frame grew with the length of the conversation.
    pub(crate) render_cache: Vec<Option<crate::ui::BlockRender>>,
    /// Which attempt the trailing retry block is on, and the cap. Shown as
    /// `retry N/M` so collapsing the sequence still tells the user the agent
    /// is working through its budget rather than stuck.
    pub(crate) retry_attempt: u32,
    pub(crate) retry_max: u32,
    /// When a turn that failed because the provider was down is continued
    /// by itself, and how many times that has happened since a turn last
    /// finished.
    pub(crate) auto_retry: Option<Instant>,
    pub(crate) auto_retries: u32,
    /// How many times the trailing error block's message has arrived in a row.
    /// Shown as `×N` so a silent collapse does not hide that it is still
    /// happening. Reset whenever a non-error block lands.
    pub(crate) error_repeats: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct TextSelection {
    pub anchor: (u16, u16), // (row, col)
    pub head: (u16, u16),
}

impl App {
    pub(crate) fn new(config: Config) -> Self {
        let theme = Theme::find(&config.ui.theme);
        let show_sidebar = config.ui.show_sidebar;
        let context_window = config.model.context_window;
        let workspace = config.workspace();
        // The interface can put a question to the user, so the agent
        // holding the conversation may ask one.
        let agent = Agent::new(config.clone()).asking_user().into_shared();
        let background = vec![agent.background_events()];
        // Started at launch, in the background: the first message used to
        // wait for every MCP server in turn, 33 seconds with one that hung.
        agent.start_mcp();
        let discovery = agent.discovery();
        Self {
            agent,
            background,
            reports: Vec::new(),
            catalog_seen: 0,
            settings: SettingsDraft::default(),
            config,
            store: SessionStore::default(),
            blocks: Vec::new(),
            input: String::new(),
            cursor: 0,
            session_id: None,
            title: String::new(),
            role: Role::Orchestrator,
            // A fresh session has no agent recorded yet, so the starting
            // agent is whatever core resolves the default role to.
            agent_name: enowx_core::Session::new(Role::Orchestrator).agent_or_default(),
            switch_markers: Vec::new(),
            busy: false,
            abandoned: 0,
            delegations: Vec::new(),
            viewing: None,
            delegation_rects: Vec::new(),
            delegation_window: None,
            delegation_list_area: None,
            delegation_slide_rects: Vec::new(),
            home_started: None,
            logs: crate::logs::Logs::default(),
            log_filter: 0,
            log_detail: false,
            started: Instant::now(),
            trimmed_count: 0,
            trimmed_saved: 0,
            typesafe_check: None,
            cancel: None,
            events: None,
            task: None,
            show_reasoning: false,
            reasoning_seq: 0,
            show_tool_output: true,
            tool_expanded: std::collections::HashMap::new(),
            tool_before: std::collections::HashMap::new(),
            tool_header_markers: Vec::new(),
            tool_header_rects: Vec::new(),
            transcript_area: None,
            composer_field: None,
            composer_offset: 0,
            composer_width: 0,
            scroll: 0,
            max_scroll: 0,
            auto_scroll: true,
            should_quit: false,
            status: "ready".into(),
            tokens_in: 0,
            tokens_out: 0,
            modal: Modal::None,
            modal_cursor: 0,
            modal_items: Vec::new(),
            palette_cursor: 0,
            palette_offset: 0,
            modal_offset: 0,
            modal_scrolled: false,
            modal_offset_for: Modal::None,
            update: Default::default(),
            update_shown: Default::default(),
            update_heard: false,
            branch_usage: std::collections::HashMap::new(),
            composer_palette: None,
            composer_palette_rows: Vec::new(),
            question: None,
            question_rows: Vec::new(),
            last_answer: None,
            field_cursor: 0,
            picker: model_picker::ModelPicker::default(),
            picking_for_agent: None,
            provider_ids: Vec::new(),
            modal_error: String::new(),
            activity: Activity::Idle,
            activity_since: Instant::now(),
            turn_started: Instant::now(),
            theme,
            sidebar_tab: 0,
            sidebar_page: 0,
            sidebar_pages: 1,
            sidebar_view: 1,
            workspace,
            show_sidebar,
            tool_counts: HashMap::new(),
            attachments: Vec::new(),
            queued: std::collections::VecDeque::new(),
            queue_paused: false,
            queue_send_button: None,
            main_area: None,
            page_tabs: Vec::new(),
            page_index: 0,
            settings_nav: false,
            settings_sections: Vec::new(),
            settings_content: None,
            resources: crate::resources::Sampler::default(),
            handoff: None,
            mcp_config_stamp: mcp_config_mtime(),
            mcp_reload_checked: std::time::Instant::now(),
            attach_error: None,
            sidebar_pages_area: None,
            sidebar_area: None,
            context_tokens: 0,
            context_window,
            mcp_field: 0,
            popup_rows: Vec::new(),
            mcp_field_rows: Vec::new(),
            last_wheel: None,
            quit_confirm_yes: false,
            quit_confirm_rects: [(Rect::default(), false), (Rect::default(), false)],
            stop_target: None,
            user_block_markers: Vec::new(),
            user_block_rects: Vec::new(),
            message_target: None,
            message_draft: String::new(),
            message_draft_cursor: 0,
            file_link_markers: Vec::new(),
            render_cache: Vec::new(),
            retry_attempt: 0,
            retry_max: 0,
            auto_retry: None,
            auto_retries: 0,
            error_repeats: 0,
            file_link_rects: Vec::new(),
            selection: None,
            wrapped_snapshot: Vec::new(),
            popup_body: None,
            sidebar_tabs: Vec::new(),
            modal_rows: Vec::new(),
            discovery,
            modal_search: String::new(),
            mcp_draft: crate::modal::McpDraft::default(),
        }
    }

    /// The blocks a turn's events belong to: always the main conversation,
    /// even while a sub-agent's transcript is the thing on screen.
    ///
    /// `self.blocks` is what gets drawn, so during viewing it holds the
    /// branch. Writing a turn's events there put them in the sub-agent's
    /// transcript, where they were discarded on the way back — the reply
    /// simply vanished.
    pub(crate) fn conversation_mut(&mut self) -> &mut Vec<TranscriptBlock> {
        match self.viewing.as_mut() {
            Some(viewing) => &mut viewing.blocks,
            None => &mut self.blocks,
        }
    }

    pub(crate) fn push(&mut self, kind: TranscriptKind, text: impl Into<String>) {
        let text = text.into();
        // Errors collapse instead of stacking. A provider that is down, a bad
        // key, or a retry loop otherwise fills the transcript with the same
        // line over and over and pushes the real conversation off screen.
        if matches!(kind, TranscriptKind::Error | TranscriptKind::Retry) {
            self.push_failure(kind, text);
            return;
        }
        self.conversation_mut().push(TranscriptBlock { kind, text });
    }

    /// Record an error as ONE block.
    ///
    /// A repeat of the message already showing updates that block in place and
    /// bumps its counter; a different message replaces it, because the newest
    /// failure is the one worth reading and the previous one is usually its
    /// cause rather than separate news. Any other block arriving in between
    /// ends the run, so an error from an earlier turn stays where it happened.
    fn push_failure(&mut self, kind: TranscriptKind, text: String) {
        // A retry that ends in failure should leave ONE block behind, not a
        // retry line plus an error line, so a terminal error takes over the
        // retry block it grew out of.
        if let Some(last) = self.conversation_mut().last_mut() {
            if matches!(last.kind, TranscriptKind::Error | TranscriptKind::Retry) {
                let same_kind = std::mem::discriminant(&last.kind) == std::mem::discriminant(&kind);
                if same_kind && last.text == text {
                    self.error_repeats = self.error_repeats.saturating_add(1);
                } else {
                    last.kind = kind;
                    last.text = text;
                    self.error_repeats = 1;
                }
                return;
            }
        }
        self.error_repeats = 1;
        self.conversation_mut().push(TranscriptBlock { kind, text });
    }

    pub(crate) fn set_activity(&mut self, activity: Activity) {
        if self.activity != activity {
            self.activity = activity;
            self.activity_since = Instant::now();
        }
    }

    pub(crate) fn spinner(&self) -> &'static str {
        let frame = self.activity_since.elapsed().as_millis() / 120;
        SPINNER[frame as usize % SPINNER.len()]
    }

    /// Which agent holds the session right now.
    ///
    /// A plain read: `agent_name` is resolved through core's
    /// `Session::agent_or_default` whenever it can change, so the footer and
    /// sidebar do not each pay for that resolution on every frame.
    pub(crate) fn active_agent(&self) -> &str {
        &self.agent_name
    }

    /// Switch agents on the user's say-so.
    ///
    /// A forced switch overrides whatever the orchestrator decided: the user
    /// asking for a specialist by name is a stronger signal than the model's
    /// classification, and `auto_switch` does not gate it — it governs the
    /// orchestrator's own switches, not the user's.
    pub(crate) fn force_agent(&mut self, name: &str) -> anyhow::Result<()> {
        let name = name.trim().to_ascii_lowercase();
        // `/agent router` still means the orchestrator.
        let name = enowx_core::agent_def::canonical_name(&name).to_owned();
        if !self
            .discovery
            .agents
            .iter()
            .any(|a| a.name == name && a.is_routable())
        {
            let known: Vec<&str> = self
                .discovery
                .agents
                .iter()
                .filter(|a| a.is_routable())
                .map(|a| a.name.as_str())
                .collect();
            anyhow::bail!("no agent named `{name}`. Available: {}", known.join(", "));
        }
        if name == self.agent_name {
            self.status = format!("already {name}");
            return Ok(());
        }
        // Persist when there is a session to persist to; before the first
        // message there is none, and the choice still has to hold for when
        // the session is created.
        if let Some(id) = self.session_id.clone() {
            if let Ok(mut session) = self.store.load(&id) {
                session.switch_agent(&name, enowx_core::session::USER_SWITCH_REASON);
                let _ = self.store.save(&session);
            }
        }
        self.push(
            crate::session::TranscriptKind::Notice,
            format!("→ {name} · {}", enowx_core::session::USER_SWITCH_REASON),
        );
        self.agent_name = name.clone();
        self.status = format!("agent: {name}");
        Ok(())
    }

    /// Adopt a session's agent and handover history.
    ///
    /// The resolution from the legacy `role` stays in core so there is one
    /// copy of it to keep right once the loop stops setting `role` at all.
    pub(crate) fn adopt_agent(&mut self, session: &enowx_core::Session) {
        self.agent_name = session.agent_or_default();
    }

    /// The model the active agent runs on: its own when it has one that can
    /// run, otherwise the conversation's. Core resolves the turn's model the
    /// same way, so the footer always names what is answering.
    pub(crate) fn running_model(&self) -> String {
        let name = self.active_agent();
        let tier = self
            .discovery
            .agents
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.tier)
            .unwrap_or_default();
        self.config.running_model(name, tier)
    }

    /// Whether the active agent runs on a model of its own rather than the
    /// conversation's.
    pub(crate) fn agent_has_own_model(&self) -> bool {
        let name = self.active_agent();
        let tier = self
            .discovery
            .agents
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.tier)
            .unwrap_or_default();
        self.config.agent_has_own_model(name, tier)
    }

    /// Model name shown in the composer footer: the model in use, with its
    /// provider.
    pub(crate) fn model_label(&self) -> String {
        let running = self.running_model();
        let m = running.as_str();
        if m.is_empty() {
            "no model".to_string()
        } else if m != self.config.model.active.trim() {
            // The agent's own model: the conversation's effort is not its.
            m.to_string()
        } else if !self.config.model.effort.is_empty() {
            // The effort beside the model it applies to.
            format!("{m} · effort {}", self.config.model.effort)
        } else if !self.config.model.efforts.is_empty() {
            // A model that can think harder, left at its provider's default.
            format!("{m} · effort default")
        } else {
            m.to_string()
        }
    }
}

/// Format an elapsed second count as `s` / `m s` / `h m` so a long turn does
/// not display `3612s`. Never shows leading zeros; anything under a minute
/// stays raw seconds.
/// The newest change time across the files that configure MCP. `None` when
/// none exist yet.
fn mcp_config_mtime() -> Option<std::time::SystemTime> {
    [
        enowx_core::builtin_mcp::config_path(),
        enowx_core::discovery::overrides_path(),
        enowx_core::auth::auth_path(),
    ]
    .iter()
    .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
    .max()
}

pub(crate) fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

// Restore the original App impl block; the fmt_elapsed + fmt_tests split it.
impl App {
    pub(crate) fn refresh_discovery(&mut self) {
        self.discovery = self.agent.discovery();
    }
    /// Once a fresh models.dev catalogue is in, look the model in use up
    /// again: at start it was looked up in the cached one, which may have been
    /// old or empty, and a window of 128k shown for a model of 1M.
    pub(crate) fn catch_up_with_catalog(&mut self) {
        let fetched = enowx_core::catalog::REFRESHED.load(std::sync::atomic::Ordering::SeqCst);
        if fetched == self.catalog_seen || self.busy {
            return;
        }
        self.catalog_seen = fetched;
        let active = self.config.model.active.clone();
        if active.is_empty() {
            return;
        }
        let mut next = self.config.clone();
        if !next.use_model(&active) {
            return;
        }
        let (was, now) = (&self.config.model, &next.model);
        if was.context_window != now.context_window
            || was.efforts != now.efforts
            || was.price_input != now.price_input
            || was.price_output != now.price_output
            || was.vision != now.vision
        {
            self.adopt(next);
        }
    }

    /// The fields of the open form. BuiltinMcp's depend on which server is
    /// being configured; every other form has a static table.
    pub(crate) fn current_form_fields(&self) -> &'static [crate::modal::SettingsField] {
        if self.modal == crate::modal::Modal::BuiltinMcp {
            crate::modal::builtin_mcp_fields(&self.settings.provider_id)
        } else if self.modal == crate::modal::Modal::Rag {
            crate::modal::rag_fields(&self.settings.rag_provider, &self.settings.database_backend)
        } else if self.modal == crate::modal::Modal::Team {
            crate::modal::team_fields(self.settings.team_enabled == "on")
        } else if matches!(
            self.modal,
            crate::modal::Modal::General | crate::modal::Modal::Display
        ) {
            self.prefs_fields()
        } else if self.modal == crate::modal::Modal::Updates {
            &[
                crate::modal::SettingsField::UpdateCheck,
                crate::modal::SettingsField::UpdateAuto,
            ]
        } else {
            crate::modal::form_fields(self.modal)
        }
    }

    /// If an MCP config file changed on disk (an agent filling in credentials
    /// with `enx mcp set`), reload the servers in place. Checked about once a
    /// second, not every frame: three stat calls on the render thread, times
    /// 25 frames a second, is a cost for nothing when nothing changed.
    pub(crate) fn tick_mcp_reload(&mut self) {
        if self.mcp_reload_checked.elapsed() < std::time::Duration::from_millis(1000) {
            return;
        }
        // On the same once-a-second beat: finished delegations whose
        // transcript is gone leave the sidebar.
        self.prune_delegations();
        self.mcp_reload_checked = std::time::Instant::now();
        let now = mcp_config_mtime();
        if now != self.mcp_config_stamp {
            self.mcp_config_stamp = now;
            self.adopt(self.config.clone());
            self.status = "mcp servers reloaded".into();
        }
    }

    pub(crate) fn adopt(&mut self, config: Config) {
        self.theme = Theme::find(&config.ui.theme);
        self.show_sidebar = config.ui.show_sidebar;
        self.context_window = config.model.context_window;
        self.config = config.clone();
        self.agent = Agent::new(config).asking_user().into_shared();
        self.background.push(self.agent.background_events());
        // MCP servers start now, in the background, so the next message
        // does not wait on them.
        self.agent.start_mcp();
        self.refresh_discovery();
    }

    /// Whether the window shows the home screen: a new conversation with
    /// nothing in it yet. The first block, a message or a command's notice,
    /// brings the chat layout; a resumed session opens straight into it.
    pub(crate) fn is_home(&self) -> bool {
        self.session_id.is_none() && self.blocks.is_empty() && self.viewing.is_none()
    }

    /// Drop the click targets the last transcript drew. Once it is off
    /// screen nothing it showed can be clicked: after `/new`, a click where a
    /// file link used to be opened that file from the old conversation.
    pub(crate) fn forget_transcript_targets(&mut self) {
        self.tool_header_markers.clear();
        self.tool_header_rects.clear();
        self.file_link_markers.clear();
        self.file_link_rects.clear();
        self.user_block_markers.clear();
        self.user_block_rects.clear();
        self.delegation_rects.clear();
        self.transcript_area = None;
    }

    pub(crate) fn new_session(&mut self) {
        self.blocks.clear();
        self.switch_markers.clear();
        // The Agents tab lists this session's sub-agents, not the last one's.
        self.delegations.clear();
        self.delegation_window = None;
        self.adopt_agent(&enowx_core::Session::new(self.role));
        self.session_id = None;
        self.title.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.context_tokens = 0;
        self.tool_counts.clear();
        self.events = None;
        self.auto_scroll = true;
        self.scroll = 0;
        self.status = "new session".into();
    }
}

#[cfg(test)]
mod fmt_tests {
    use super::fmt_elapsed;
    #[test]
    fn rolls_over_at_minutes_and_hours() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(45), "45s");
        assert_eq!(fmt_elapsed(60), "1m 0s");
        assert_eq!(fmt_elapsed(135), "2m 15s");
        assert_eq!(fmt_elapsed(3600), "1h 0m");
        assert_eq!(fmt_elapsed(4923), "1h 22m");
    }
}
