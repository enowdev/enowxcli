# Changelog

All notable changes to enowx. Dates are YYYY-MM-DD.

## Unreleased

### Code search database

- **RAG selects a persistent embedded PostgreSQL database by default** on Linux GNU x86_64/aarch64, macOS arm64 and Windows x86_64. `enowx mcp set rag --database postgres --dsn ...` selects external PostgreSQL; a stored legacy DSN without a saved backend continues to select PostgreSQL. Unsupported release targets remain PostgreSQL-only. The embedded owner is shared across enowx processes and shuts down after 60 idle seconds; Windows uses an ephemeral loopback listener with trust authentication, accessible to local processes that can connect.

## v0.2.2 (2026-10-03)

### Renamed to enowx

- **The command is now `enowx`, not `enx`.** The project name and the binary
  are `enowx` everywhere; `enx auth login` becomes `enowx auth login`, and so
  on. The install scripts, release archives (`enowx-<target>`), docs and the
  website use `enowx`. Your data in `~/.enx/` is left where it is, so settings,
  keys and sessions carry over; only the command you type changed. Old
  `ENX_*` environment knobs still work alongside the new `ENOWX_*` ones.

### Models and agents

- **A model manager that tests before it assigns.** A new `model-manager`
  skill (carried by the orchestrator and maestro) helps set a model per agent:
  `enowx models list` shows the connected providers and their models, and
  `enowx models test provider/model` makes one tiny call and reports whether
  it answered, how fast, and the tokens. The skill lists what you own,
  brainstorms a mapping, tests every candidate, and sets only the ones that
  answered.
- **The footer and `/model` follow the active agent's model.** Setting an
  agent to another model shows at once; picking a model while an agent has its
  own changes that agent's model, not the conversation's.
- **Each delegated sub-agent shows the model it runs on**, in the sidebar list
  and the log.
- **Stopping a sub-agent asks first** (a right-click opens a confirm that
  defaults to keeping it).

### Agents

- **Brainstorm no longer asks how the work is built.** It asks only what is
  yours to decide (purpose, scope, look, content) and reads the one relevant
  skill before offering options, so it never invents tool choices or asks you
  to do a step a tool can do.
- **A `motion-video` skill**: render a video file (an explainer, an animated
  logo) from code with synthesised sound, instead of reaching for video
  software or asking you to record.


## v0.2.1 (2026-10-03)

### Interface work

- **Everything that can be set is in Settings.** New sections: General
  (browser preview, language server checks, background delegation,
  switching agent by itself, compaction and its threshold, model calls per
  turn, the shell timeout, the model for each tier), Display (sidebar,
  currency and rate) and TypeSafe. Each field is saved through the same
  checks as `enx config set`, and a bad value is refused with why.

- **Structure before styling: `ui-structure` and `ui-anatomy`.** The
  frontend, canvas and mobile agents now write a blueprint before any
  code: the screen's job and its one primary action, the regions its kind
  of page needs in order (homepage, dashboard, list, detail, settings,
  form, sign-in, pricing, docs, article, checkout) with why each is there,
  the skeleton in numbers (container, gutters, grid, spacing, what each
  breakpoint changes), and the components with their states. They build
  in a fixed order (tokens, layout primitives, components, sections, the
  page) and every control meets its contract in `ui-anatomy`: parts,
  variants, sizes, every state, keyboard.
- **Reviewers judge structure before taste.** Content touching the
  window's edge, sections on different left edges, mixed surfaces, cut-off
  phone layouts, selected states shown by a shade, invalid submits and
  missing loading, empty or error states rule out `Good`; a page not seen
  in a browser is never called good to look at.
- **`preview` reports more**: content touching the window's edge (no
  gutter), content cut off past it, and a theme toggle with text on it.

### Agents and delegation

- **The orchestrator and maestro can stop a delegation** at work with
  `stop_delegation` and a reason; its report says who stopped it and why,
  and the list of delegations still running now gives their sessions.
- **Each delegated sub-agent shows the model it runs on** in the sidebar
  list and the log, so a glance shows what is answering each one.
- **Stopping a sub-agent asks first.** A right-click in the sidebar used to
  stop it at once; it now opens a confirm that defaults to keeping it, so a
  stray click stops nothing.

### Models

- **A model manager that tests before it assigns.** A new `model-manager`
  skill, carried by the orchestrator and maestro, helps set a model per
  agent: `enx models list` shows the connected providers and their models,
  the model in use and the per-tier and per-agent ones; `enx models test
  provider/model` makes one tiny call and says whether it answered, how
  fast, and the tokens, failing with the provider's own error. The skill
  lists what you own, brainstorms a mapping, tests every candidate, then
  sets only the ones that answered.
- **The footer and `/model` follow the active agent's model.** Setting an
  agent to another model used to leave the footer on the old one until the
  next `/model`, and picking a model while an agent had its own changed the
  conversation's model, not the agent's. Both now track the agent that is
  answering.
- **`antares --version` and `--help` work as commands** (the CLI took a
  leading flag as an option of the default serve).

## v0.2.0 (2026-10-03)

Still an early release: expect rough edges, and please report them in
Issues. The highlights: settings move to a Settings tab beside the chat;
agents can work together (messages, a shared board, cross-review) when you
turn it on; built-in code search over your project (`rag`); built-in MCP
servers for Coolify, Dokploy and your VPSes; a stopped or failed sub-agent
can be read back by the agent that sent it; every shortcut now works on
Linux, macOS and Windows; and enx updates itself (`enx update`, `/update`).

### Agents and delegation

- **Sub-agents count, and are logged.** What a delegated agent uses comes
  up to the interface as it works: the Log tab lists every agent's model
  calls, retries, trims and errors, named; the SESSION card's new `used`
  row and the cost count the whole session, sub-agents included (a
  reviewer's tokens too); and with a sub-agent's transcript open, the
  context shown is that agent's own.
- **A finished sub-agent leaves the sidebar** once its transcript is
  cleared, so no row leads nowhere; one at work, failed or stopped stays.
- **One reviewer across cross-review rounds.** It waits between rounds in
  its own session (so the delegate can answer it) and carries on there,
  keeping its context and the prompt cache.

- **Stop a sub-agent with a right-click on it in the sidebar.** Its caller
  gets a report that it was stopped by the user, with the files it had
  changed and what it said last, and is woken by it like any report; its
  transcript is kept.
- **The orchestrator and maestro can read what a delegation did**
  (`delegation_log`): what it was asked, said and called, so after one is
  stopped or fails the next brief starts where that work stopped.

- **Agents can work together (Settings > Team, off by default).** Agents at
  work at the same time message each other (`message_agent`), share a board
  for the run (`team_board`), and a delegate's work that changed files is
  cross-reviewed: the reviewer's corrections go back to the delegate and the
  work is checked again, up to the rounds set (1 to 5). Each part can be
  turned off on its own.

- **New `maestro` agent**: an all-rounder in the LEAD group. It has the
  orchestrator's reach (delegate and hand off to anyone) but carries every
  tool, so it can change files, run commands and check interfaces itself
  rather than only handing work out. For a job that mixes doing and
  delegating. The orchestrator stays the default; pick it with
  `/agent maestro`.
- **A delegated agent always comes back with a report.** The four fields
  (DONE, CHANGED, VERIFIED, NEXT) are now read however the model dresses them
  (markdown, changed case); every ending without a report is nudged; after two
  nudges the model is asked once more with no tools, so all it can do is write
  the report; and if it still will not, the caller gets the four fields built
  from what the branch actually did.
- **A delegation at work is never run twice.** Resuming a branch that is still
  running is refused, and the orchestrator is told up front what it left
  running, so a "continue" from the user can no longer start the same work a
  second time.
- **A finished delegation is cleaned up.** Once a delegation reports
  successfully its report stays in the conversation and its transcript (and any
  delegation under it) is deleted instead of filling the disk. One that failed
  or stopped short is kept, to be resumed.
- **Reviewers are firm, tidy and honest.** `review` and `perf` open with one
  verdict on a fixed scale (Good, Good with fixes, Needs work, Bad) that
  follows from the findings, call bad work bad and good work good with the
  reason, and treat inconsistency as a finding rather than a nit to drop.
- **Give an agent its own model from the roster.** In `/agent`, `m` opens the
  model list for the selected agent and `d` puts it back on the default.
  Every agent shows the model it runs on under its description.

### Interface

- **enx updates itself.** `enx update` installs the latest release in place
  of the running binary after checking its published checksum (`--check`
  only looks, `--force` reinstalls). The interface looks for a new release
  in the background at start and notes it in the status bar until
  `/update` installs it. Settings > Updates turns the check off, or has it
  install by itself, used from the next start.
- **Line edits show as diffs.** An edit by line anchors used to show the
  tool's raw anchored text; it now returns a small diff the interface draws
  like any edit.

- **Chat and Settings tabs at the top right.** Settings takes the whole main
  column in place of the chat: its sections (Models, Providers, Agents, Team,
  MCP, RAG, Skills, Sessions, Theme) are listed on the left and the chosen one is
  beside them. `Ctrl+P` switches between Chat and Settings. Settings opens
  on the section list: `Up`/`Down` pick a section, `Enter` goes in, and `Esc`
  steps back one level (form to list, section to section list, list to
  chat); a click on a section picks it, a click inside goes in. The searchable command list
  moved to `/commands` (typing `/` still lists commands inline).
- **Queue messages typed while a turn runs.** Enter during a turn puts the
  message in a queue above the composer, with a `[send now]` button; queued
  messages go one at a time as each turn ends. `Ctrl+Enter` or `Ctrl+S` sends
  now, `Shift+Enter` or `Alt+Enter` is a newline at any time, and `Up` in an
  empty composer edits the last queued message.
- **`/handoff` to a fresh session.** Folds the conversation into a summary in a
  new, light session held by the same agent; asks first whether to keep the old
  session's history or delete it.
- **The SESSION card shows what the session costs the machine**: the memory and
  CPU of enx and the processes it started (MCP servers, language servers, the
  preview browser), and the history on disk.
- **A sub-agent's transcript is read only.** Viewing a delegation shows no
  composer; nothing is typed or sent from it.
- **The wheel scrolls, a row at a time, and never moves a selection.** In
  any list (models, MCP, skills, sessions, the command list, a settings form)
  the wheel scrolls the view and leaves the selection where it is; the next
  key brings the view back to it. The side card scrolls by lines instead of
  turning pages, its position shown as `▲ 21-40/69 ▼`. In Settings the wheel
  over a section scrolls it at once, without Enter.
- **Every shortcut works on Linux, macOS and Windows.** Each one has a form
  that reaches enx through the terminals that swallow the first:
  - `Ctrl+S` sends now where a terminal cannot tell `Ctrl+Enter` from Enter;
    `Shift+Enter` is a newline where it can.
  - Terminals that speak the kitty keyboard protocol (kitty, WezTerm, foot,
    Ghostty, Alacritty, iTerm2) are asked to use it, so those keys are told
    apart there.
  - `Alt+Up` opens the message menu (macOS takes `Ctrl+Up`), `Alt+V`
    attaches from the clipboard (Windows Terminal takes `Ctrl+V`), and
    `Alt+B`/`Alt+F` page the sidebar (macOS sends them for Option+arrows).
  - AltGr characters (`@`, `{`, `€` on many European layouts) type on
    Windows instead of being read as Ctrl+Alt shortcuts.
  - A multi-line paste on Windows arrives as one paste instead of sending
    each line as its own message.
  - `Ctrl+Backspace` and `Alt+Backspace` erase a word; `Ctrl+D` quits only
    from an empty composer and erases forward otherwise; an `Alt` chord no
    longer types its letter.
- **Ctrl shortcuts instead of function keys**, since not every terminal passes
  F-keys through: `Ctrl+T` next sidebar tab, `Ctrl+G`/`Ctrl+X` the log's filter
  and detail, and in `/model` `Ctrl+N` add, `Ctrl+E` edit, `Ctrl+R` refresh.

### Models

- **Model names are matched however the upstream spells them.** `claude-opus-4.7`,
  `claude-opus-4.7-1m`, `anthropic/claude-opus-4.7:thinking`,
  `us.anthropic.claude-opus-4-7` and the like now resolve to the right
  catalogue entry, so context window, prices, vision and thinking efforts are
  correct. The maker's listing wins over a reseller's.
- **Edit a model's properties** (context window, thinking effort, vision,
  prices) from the model list.

### Built-in MCP servers

- **`coolify`, `dokploy`, `vps` and `rag`, served by enx itself** (no Node or
  Python). All four are listed in `/mcp`, off by default; `Tab` turns one on,
  and `c` opens its setup form. Credentials can also be set from the CLI
  (`enx mcp set coolify --url ... --token ...`, `enx vps add`), which an agent
  can run for you, and the running session reloads MCP the moment they land:
  no restart. `enx mcp clear <name>` forgets a setup. Tokens and passwords go
  to `auth.json`; secret fields and environment values are redacted from tool
  output; a VPS host key is pinned on first connection.
- **RAG indexes by itself and cuts code along its syntax.** The workspace
  is indexed when a session starts, a file an agent writes or edits is
  indexed the moment the tool returns, other changes every half minute, and
  everything right before each search, so agents no longer call `index`
  first. Indexing runs at low OS priority and holds its full scan while a
  turn runs.
  Chunks follow the syntax tree: a function, type, method or config key is
  whole, with its comments, and carries where it sits and what it defines;
  only an item over about 4,000 characters is split, along its own body.
  Markdown is cut at headings, other text at paragraphs. Code that only
  moved keeps its embedding. Turn auto-indexing off in Settings > RAG or
  with `--auto-index off`.
- **`vps` signs in the way `ssh` does.** Encrypted key files (with their
  passphrase), PuTTY `.ppk` keys, ssh-agent (Pageant and the OpenSSH agent on
  Windows), hosts written as `~/.ssh/config` aliases (their `HostName`,
  `User`, `Port` and `IdentityFile`), the default `~/.ssh/id_*` keys, and
  keyboard-interactive answered with the stored password. A failed sign-in
  says what was tried and why each was refused, and a prompt for a one-time
  code is reported rather than answered.
- **Built-in code search (`rag`), with a Settings section of its own.**
  Indexes the workspace into Postgres with pgvector, local or cloud, and
  searches it with dense and keyword matches fused, then reranked. Settings
  > RAG (or `/rag`) turns it on and picks the database and the embeddings:
  Voyage AI, OpenAI, or any OpenAI-compatible endpoint (Ollama, LM Studio,
  Jina, a gateway), with the model, the vector width and the reranker.
  `enx mcp set rag` takes the same as flags. A change of model makes the
  next `index` embed the project again rather than mix vectors. Indexing is
  incremental and honours `.gitignore`; secrets and lockfiles are never
  indexed. Off by default, and its skill reaches the agents only while it is
  on.

### Skills

- **A theme toggle is an icon, with no text.** `ui-themes`, `ui-part-header`
  and `canvas-data` now say so: a sun or moon button, named by its
  `aria-label` alone.

- **Desktop apps in Rust: `systems-desktop` and `systems-gpui`.** The first
  chooses the stack (Tauri for a web UI with a Rust core, GPUI for a native
  GPU-drawn app, with egui, iced and Slint in brief) and settles packaging,
  signing and updates up front. The second covers GPUI, the framework Zed
  is built on: setup per platform, entities and views, `notify`, events,
  actions, key bindings and focus, `uniform_list`, async work, testing and
  shipping. Carried by `systems` and `fe`.

- **New `ui-layout-grid` skill**: CSS Grid and Flexbox mechanics for tidy
  layouts.
- **New `canvas` agent and skills**: standalone single-file HTML pages, tools,
  visualisers and games.
- **UI skills enforce layout mechanics**: every layout is flexbox or grid,
  cards in a group share one size and row height, controls in a row share a
  height, and dropdowns are custom because the native select renders
  differently on every OS (with a keyboard- and ARIA-complete custom select in
  `ui-part-choices`).

### Tools

- **The `todo` tool keeps nested steps** instead of silently dropping them: a
  step may be an object with sub-steps, flattened with indentation.
- **The `ask` tool reads a call however the model dresses its fields** (a
  question named `prompt`/`text`/`q`/`title`, options named `choices`/`answers`,
  a bare string, a single question not wrapped in a list).

### Fixes

- **Backspace works everywhere.** Terminals that send Backspace as `^H` (many
  Linux terminals and SSH sessions) now erase instead of doing nothing.
- **Windows builds.** `nix` is used only on Unix, and the bash tool and preview
  browser fall back to the `sh` from Git for Windows, or PowerShell.
- **The session resource card counts only enx's real descendants**, not
  unrelated processes caught by macOS recycling a pid.
- **`/resume` lists conversations only**, never a delegation's transcript.
- **Enter saves a built-in MCP server's setup form.** It did nothing before;
  only the CLI could save one.
- **Long files are written in parts.** A reply of several hundred lines
  takes minutes and the provider could cut it off mid-call, so nothing was
  written (the `canvas` agent failed this way repeatedly). `write`, the
  harness and the canvas prompt now ask for at most about 150 lines per
  call, continued with `edit` from a marker comment.
- **The `write` tool refuses an empty write** with a message telling the
  model to send the file in parts, instead of silently creating an empty file
  when a long reply was cut off.
- **No more freeze on Windows.** The session resource card measured
  processes on the interface thread, which on Windows could take long enough
  to stop scrolling and typing; it now measures in the background.

### Project

- **Install from [enowx.ai](https://enowx.ai)** (`curl -fsSL https://enowx.ai/install.sh | sh`,
  or `irm https://enowx.ai/install.ps1 | iex` on Windows). The installer adds
  enx to your `PATH` and the site counts installs.
- **Licensed under Apache-2.0** (was MIT).

## v0.1.0 (2026-10-02)

First public build (early release). Prebuilt binaries for macOS, Linux
(static musl) and Windows on x86_64 and aarch64, built and published by a
GitHub release workflow. The agent loop, the terminal interface, roles, the
specialist roster and the authorized-assessment security team, built-in
skills, and smart context-window detection.
