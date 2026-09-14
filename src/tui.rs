mod layout;

use layout::{
    CODE_WIDTH, COUNT_WIDTH, ColumnSpec, DURATION_WIDTH, EFFORT_WIDTH, ENDPOINT_WIDTH, ERROR_WIDTH,
    HIT_WIDTH, ID_WIDTH, LayoutTier, MISS_WIDTH, MODEL_MEDIUM_WIDTH, MODEL_NARROW_WIDTH,
    MODEL_WIDE_WIDTH, PROJECT_MEDIUM_WIDTH, PROJECT_WIDE_WIDTH, PROVIDER_WIDTH, RATE_WIDTH,
    SESSION_TREE_ID_WIDTH, STATUS_WIDTH, TIME_WIDTH, TOKEN_WIDTH,
};

use std::{
    collections::HashMap,
    io::{self, Stdout},
    sync::mpsc,
    time::{Duration, SystemTime},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::sync::oneshot;

use crate::{
    monitor::{
        ActiveRequest, CacheWriteQuality, CompletedRequest, ConversationSummary, LOCAL_PROVIDER,
        MockMonitor, ModelUsage, MonitorHandle, MonitorState, QualityCoverage, QualityFields,
        RequestCache, SESSION_TOKEN_BUCKET_SECS, SIDE_CONVERSATION_SUFFIX, SessionCacheStats,
        SessionSummary, UnattributedUsage, UsageEvidence, UsageQuality,
    },
    paths,
    registry::Registry,
};

const TEAL: Color = Color::Rgb(78, 201, 176);
const WHITE: Color = Color::Rgb(240, 244, 248);
const DIM_WHITE: Color = Color::Rgb(180, 190, 200);
const SEPARATOR: Color = Color::Rgb(72, 74, 82);
const BG: Color = Color::Rgb(18, 18, 22);
const PANEL_BG: Color = Color::Rgb(22, 22, 27);
const SELECTED_BG: Color = Color::Rgb(42, 45, 54);
const GREEN: Color = Color::Rgb(120, 200, 120);
const RED: Color = Color::Rgb(220, 120, 120);
const YELLOW: Color = Color::Rgb(220, 200, 100);
const BLUE: Color = Color::Rgb(120, 170, 230);
const PURPLE: Color = Color::Rgb(190, 140, 240);
const DIM: Color = Color::Rgb(100, 104, 114);
const SESSION_SPARKLINE_MIN_WIDTH: u16 = 190;
const SESSION_SPARKLINE_MAX_TOKENS: u64 = 4_000;

/// What a cell shows in place of a count no backend ever reported. It is not a
/// zero: the Codex backend reports no cache write at all, and a stream cut
/// short reports no output.
const MISSING_TOKENS_LABEL: &str = "n/a";
/// Marks a count that is still an estimate. A closing report may replace it,
/// upwards or downwards.
const OPENING_TOKENS_MARK: &str = "~";
/// Marks a model a request only asked for or was routed to. No outgoing
/// request was seen carrying it, so it is not evidence of what answered. The
/// request detail spells the mark out; a row has no room for the sentence.
const REQUESTED_MODEL_MARK: &str = "?";
/// What a row shows for a request the proxy answered itself. No model ran, so
/// naming one would read as a call that never happened.
const LOCAL_ANSWER_LABEL: &str = "local answer";
/// What a row shows in place of four counts nothing was metered for: a request
/// the proxy answered itself, or a token estimate. It is not an unreported
/// count either — there was nothing to report.
const UNCOUNTED_TOKENS_LABEL: &str = "no tokens counted";
/// What marks the Sessions row that stands for a whole session. Its figures
/// are every request of the session counted once; the conversation rows under
/// it are those same requests split another way, never parts to add up to it.
/// The mark goes in front so a narrow column keeps it when the id is cut.
const SESSION_AGGREGATE_MARK: &str = "Σ ";
/// The conversation Claude Code gives no agent id: the session's own thread.
const MAIN_CONVERSATION: &str = "main";
/// Marks a conversation whose parent the session never saw. Claude Code named
/// one, and the walk could not place it — the conversation is unknown here, or
/// it and its parent name each other — so the row hangs under the session like
/// a thread that named no parent at all. The mark says which of the two it is.
/// It goes in front of the label, so a column too narrow for the whole cell
/// cuts the id and keeps what says where the row belongs. It points up at the
/// parent it is about, and it is deliberately not [`REQUESTED_MODEL_MARK`],
/// which says something else about a model in another column.
const UNKNOWN_PARENT_MARK: &str = "^";
/// What a pane's title says once the row that was selected is gone. The
/// selection is not handed to whatever row took its place, so the title has to
/// account for a marker that moved or disappeared on its own.
const SELECTION_RESET_NOTICE: &str = "selection reset";
/// How that thread is named where the column has room for the words. Cutting
/// them would leave a truncation mark where the short form fits whole, so a
/// narrow column keeps [`MAIN_CONVERSATION`] instead.
const MAIN_CONVERSATION_LABEL: &str = "main thread";
/// What a cell shows in place of a model or a backend when the figures behind
/// it ran on more than one. The count is part of the text: naming the last of
/// them would read as the one every token went to. The count follows the word
/// bare rather than bracketed so the spelling fits the provider column too,
/// which is as wide as its own header.
const MIXED_MODELS_LABEL: &str = "mixed";
/// What a rollup line calls a caller that named no model at all.
const UNNAMED_MODEL_LABEL: &str = "unnamed";
/// How many per-model rows the session detail lists before saying how many it
/// left out. The pane is a share of the terminal and does not scroll.
const SESSION_MODEL_ROLLUP_LIMIT: usize = 3;
/// How many caller models one rollup line names before it says how many more
/// there were. What the row cost matters more than the tail of the list, and
/// the line is cut from the right.
const REQUESTED_MODEL_LIMIT: usize = 2;

pub struct MonitorUiConfig<'a> {
    pub listen_url: String,
    pub port: u16,
    pub registry: &'a Registry,
    pub shutdown: Option<oneshot::Sender<()>>,
    pub shutdown_complete: Option<mpsc::Receiver<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorExit {
    ShutdownComplete,
    ForceQuit,
}

pub fn run_monitor(
    handle: MonitorHandle,
    config: MonitorUiConfig<'_>,
) -> Result<MonitorExit, anyhow::Error> {
    run_monitor_loop(|| handle.snapshot(), config, None)
}

pub fn run_mock_monitor(port: u16, registry: &Registry) -> Result<(), anyhow::Error> {
    let mut monitor = MockMonitor::new();
    run_monitor_loop(
        move || monitor.snapshot(),
        MonitorUiConfig {
            listen_url: "mock://tui-demo".to_string(),
            port,
            registry,
            shutdown: None,
            shutdown_complete: None,
        },
        Some(mock_setup_text(port, registry)),
    )
    .map(|_| ())
}

fn run_monitor_loop(
    mut snapshot: impl FnMut() -> MonitorState,
    config: MonitorUiConfig<'_>,
    setup_text_override: Option<String>,
) -> Result<MonitorExit, anyhow::Error> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = MonitorApp {
        listen_url: config.listen_url,
        setup_text: setup_text_override.unwrap_or_else(|| setup_text(config.port, config.registry)),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: Selection::default(),
        recent_selected: Selection::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: config.shutdown,
        shutdown_complete: config.shutdown_complete,
    };

    let run_result = run_monitor_events(&mut terminal, &mut snapshot, &mut app);
    if run_result.is_err() {
        app.begin_shutdown();
        let state = snapshot();
        let _ = terminal.draw(|frame| render(frame, &mut app, &state));
        app.wait_for_shutdown_completion();
    }
    let cursor_result = terminal.show_cursor();
    let exit = run_result?;
    cursor_result?;
    Ok(exit)
}

fn run_monitor_events(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut snapshot: impl FnMut() -> MonitorState,
    app: &mut MonitorApp,
) -> Result<MonitorExit, anyhow::Error> {
    loop {
        let state = snapshot();
        // The rows of this snapshot, which both the panes and the keyboard
        // work over: what a pane picked is looked up in them once a frame.
        let rows = session_rows(&state.sessions);
        app.sync_selection(&rows, &state.recent);
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| render(frame, app, &state))?;
        if app.shutdown_is_complete() {
            return Ok(MonitorExit::ShutdownComplete);
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.handle_ctrl_c() {
                            return Ok(MonitorExit::ForceQuit);
                        }
                    }
                    _ if app.phase == MonitorPhase::ShuttingDown => {}
                    KeyCode::Char('y') if app.phase == MonitorPhase::ConfirmingShutdown => {
                        app.begin_shutdown()
                    }
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')
                        if app.phase == MonitorPhase::ConfirmingShutdown =>
                    {
                        app.cancel_shutdown_confirmation()
                    }
                    _ if app.phase == MonitorPhase::ConfirmingShutdown => {}
                    KeyCode::Char('q') => app.request_shutdown_confirmation(),
                    KeyCode::Char('?') => app.show_help = !app.show_help,
                    KeyCode::Char('b') => app.show_setup = !app.show_setup,
                    KeyCode::Tab => app.focus = app.focus.next(),
                    KeyCode::Down => app.move_down(&rows, &state.recent, true),
                    KeyCode::Char('j') => app.move_down(&rows, &state.recent, false),
                    KeyCode::Up => app.move_up(&rows, &state.recent, true),
                    KeyCode::Char('k') => app.move_up(&rows, &state.recent, false),
                    KeyCode::Right => app.focus = FocusPane::Recent,
                    KeyCode::Left => app.focus = FocusPane::Sessions,
                    KeyCode::Enter => {
                        // A detail pane is opened for a selected row, which a
                        // pane holding none has nothing to show for.
                        app.detail = match app.focus {
                            FocusPane::Sessions if app.selected.row().is_some() => {
                                Some(DetailView::Session)
                            }
                            FocusPane::Recent if app.recent_selected.row().is_some() => {
                                Some(DetailView::Request)
                            }
                            _ => None,
                        }
                    }
                    KeyCode::Esc => {
                        if app.show_help {
                            app.show_help = false;
                        } else if app.show_setup {
                            app.show_setup = false;
                        } else {
                            app.detail = None;
                        }
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusPane {
    Sessions,
    Recent,
}

impl FocusPane {
    fn next(self) -> Self {
        match self {
            Self::Sessions => Self::Recent,
            Self::Recent => Self::Sessions,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailView {
    Session,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorPhase {
    Running,
    ConfirmingShutdown,
    ShuttingDown,
}

/// What names a Sessions row across snapshots: a session, or one conversation
/// of a session. The conversation alone would not do it — two sessions can
/// hold the same agent id — and a row number does not do it at all.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionRowKey {
    session: Option<String>,
    /// `None` for the session's own aggregate row.
    conversation: Option<String>,
}

impl SessionRowKey {
    /// The aggregate row of the session this row belongs to: where a
    /// conversation row's selection goes when the conversation is gone.
    fn session_row(&self) -> Self {
        Self {
            session: self.session.clone(),
            conversation: None,
        }
    }
}

/// Which row of a pane is selected, held as what the row is rather than as
/// where it sits. A snapshot arrives several times a second and rows move
/// within it — sessions are ordered by id, so one seen later can arrive above
/// the selected row, a session gaining a conversation pushes every row below
/// it down, and the recent list grows from the top. A row number names a
/// different row after any of that.
struct Selection<K> {
    key: SelectionKey<K>,
    /// Where the key was found when the pane last resolved a snapshot. A
    /// cache for the renderer and for the keyboard, which both work in row
    /// numbers; the key is what decides it.
    row: Option<usize>,
    /// Set when the selected row disappeared and cleared by the next move:
    /// what the pane marks now, if anything, is not what was picked.
    reset: bool,
}

enum SelectionKey<K> {
    /// Nothing picked yet: the pane starts on its first row, as a monitor
    /// that nobody has pressed a key on always has.
    Unset,
    Picked(K),
    /// Nothing selected: the picked row disappeared and there was no row of
    /// its own session to fall back to.
    Empty,
}

impl<K> Default for Selection<K> {
    fn default() -> Self {
        Self {
            key: SelectionKey::Unset,
            row: None,
            reset: false,
        }
    }
}

impl<K: Clone + PartialEq> Selection<K> {
    fn row(&self) -> Option<usize> {
        self.row
    }

    /// What the pane draws: the row to mark, and whether that row is still the
    /// one that was picked.
    fn view(&self) -> SelectionView {
        SelectionView {
            row: self.row,
            reset: self.reset,
        }
    }

    /// Take the row the keyboard moved to. A deliberate move answers whatever
    /// the pane last said about a row going away.
    fn pick(&mut self, row: usize, key: K) {
        self.key = SelectionKey::Picked(key);
        self.row = Some(row);
        self.reset = false;
    }

    /// Point the pane at the row its key names among the `rows` of a fresh
    /// snapshot. `find` locates the picked row and `fallback` names the row to
    /// fall back to once the picked one is gone.
    fn resolve(
        &mut self,
        rows: usize,
        find: impl Fn(&K) -> Option<usize>,
        fallback: impl FnOnce(&K) -> Option<(usize, K)>,
    ) {
        let picked = match &self.key {
            // A pane nobody has picked a row in sits on the first row, as it
            // did before a key was ever pressed: nothing was chosen, so there
            // is nothing to keep, and a list that grows from the top keeps
            // showing what just arrived.
            SelectionKey::Unset => {
                self.row = (rows > 0).then_some(0);
                return;
            }
            SelectionKey::Empty => {
                self.row = None;
                return;
            }
            SelectionKey::Picked(key) => key.clone(),
        };
        if let Some(row) = find(&picked) {
            self.row = Some(row);
            return;
        }
        match fallback(&picked) {
            Some((row, key)) => self.pick(row, key),
            None => {
                self.key = SelectionKey::Empty;
                self.row = None;
            }
        }
        self.reset = true;
    }
}

/// What a pane needs to know about its selection to draw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SelectionView {
    row: Option<usize>,
    /// Whether the marked row, or the missing marker, is the pane's own doing
    /// rather than the row that was picked.
    reset: bool,
}

impl SelectionView {
    /// A pane marking a row it was told to, which is how a test names one.
    #[cfg(test)]
    fn at(row: usize) -> Self {
        Self {
            row: Some(row),
            reset: false,
        }
    }

    fn marks(&self, row: usize) -> bool {
        self.row == Some(row)
    }

    /// The pane's title, which carries the notice while it has one.
    fn title(&self, pane: &str) -> String {
        match self.reset {
            true => format!("{pane} ({SELECTION_RESET_NOTICE})"),
            false => pane.to_string(),
        }
    }
}

struct MonitorApp {
    listen_url: String,
    setup_text: String,
    show_setup: bool,
    show_help: bool,
    detail: Option<DetailView>,
    focus: FocusPane,
    selected: Selection<SessionRowKey>,
    recent_selected: Selection<String>,
    tick: usize,
    phase: MonitorPhase,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl MonitorApp {
    fn handle_ctrl_c(&mut self) -> bool {
        if self.phase == MonitorPhase::ShuttingDown {
            true
        } else {
            self.begin_shutdown();
            false
        }
    }

    fn request_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::Running {
            self.phase = MonitorPhase::ConfirmingShutdown;
        }
    }

    fn cancel_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::ConfirmingShutdown {
            self.phase = MonitorPhase::Running;
        }
    }

    fn begin_shutdown(&mut self) {
        if self.phase == MonitorPhase::ShuttingDown {
            return;
        }
        self.phase = MonitorPhase::ShuttingDown;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    fn shutdown_is_complete(&self) -> bool {
        let Some(shutdown_complete) = &self.shutdown_complete else {
            return self.phase == MonitorPhase::ShuttingDown;
        };
        match shutdown_complete.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn wait_for_shutdown_completion(&self) {
        if !self.shutdown_is_complete()
            && let Some(shutdown_complete) = &self.shutdown_complete
        {
            let _ = shutdown_complete.recv();
        }
    }

    /// Point both panes at the rows they picked in a fresh snapshot. Sessions
    /// and their conversations are one flat list of rows to navigate, so the
    /// Sessions pane selects over both.
    fn sync_selection(&mut self, sessions: &[SessionRow<'_>], recent: &[CompletedRequest]) {
        self.selected.resolve(
            sessions.len(),
            |key| sessions.iter().position(|row| row.matches(key)),
            |key| {
                // A conversation that ended leaves the session it hung under,
                // which is a row still on screen that it belonged to.
                let session = key.session_row();
                sessions
                    .iter()
                    .position(|row| row.matches(&session))
                    .map(|row| (row, session))
            },
        );
        self.recent_selected.resolve(
            recent.len(),
            |key| recent.iter().position(|request| request.request_id == *key),
            // A request the recent list dropped has no wider row to stand in
            // for it.
            |_| None,
        );
    }

    fn select_session(&mut self, row: usize, sessions: &[SessionRow<'_>]) {
        match sessions.get(row) {
            Some(selected) => self.selected.pick(row, selected.key()),
            None => self.selected = Selection::default(),
        }
    }

    fn select_recent(&mut self, row: usize, recent: &[CompletedRequest]) {
        match recent.get(row) {
            Some(selected) => self.recent_selected.pick(row, selected.request_id.clone()),
            None => self.recent_selected = Selection::default(),
        }
    }

    fn move_down(
        &mut self,
        sessions: &[SessionRow<'_>],
        recent: &[CompletedRequest],
        switch_panes: bool,
    ) {
        match self.focus {
            FocusPane::Sessions => {
                match self.selected.row() {
                    Some(row)
                        if switch_panes && row + 1 >= sessions.len() && !recent.is_empty() =>
                    {
                        self.focus = FocusPane::Recent;
                        self.select_recent(0, recent);
                    }
                    Some(row) => {
                        self.select_session(
                            row.saturating_add(1).min(sessions.len().saturating_sub(1)),
                            sessions,
                        );
                    }
                    // Nothing is marked, because the picked row went away or
                    // none was ever picked: a move starts at the top again.
                    None => self.select_session(0, sessions),
                }
            }
            FocusPane::Recent => {
                let row = self.recent_selected.row().map_or(0, |row| {
                    row.saturating_add(1).min(recent.len().saturating_sub(1))
                });
                self.select_recent(row, recent);
            }
        }
    }

    fn move_up(
        &mut self,
        sessions: &[SessionRow<'_>],
        recent: &[CompletedRequest],
        switch_panes: bool,
    ) {
        match self.focus {
            FocusPane::Sessions => {
                let row = self.selected.row().map_or(0, |row| row.saturating_sub(1));
                self.select_session(row, sessions);
            }
            FocusPane::Recent => {
                if switch_panes && self.recent_selected.row() == Some(0) && !sessions.is_empty() {
                    self.focus = FocusPane::Sessions;
                    self.select_session(sessions.len().saturating_sub(1), sessions);
                } else {
                    let row = self
                        .recent_selected
                        .row()
                        .map_or(0, |row| row.saturating_sub(1));
                    self.select_recent(row, recent);
                }
            }
        }
    }
}

impl Drop for MonitorApp {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, anyhow::Error> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut MonitorApp, state: &MonitorState) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(30),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, root[0], app, state);
    // A pane holding no selection has no row to detail, which an index past
    // the end is what the detail panes read as nothing selected.
    match app.detail {
        // A conversation row shows the detail of the session it belongs to.
        Some(DetailView::Session) => render_session_detail(
            frame,
            root[1],
            state,
            app.selected
                .row()
                .and_then(|row| selected_session_index(&state.sessions, row))
                .unwrap_or(state.sessions.len()),
        ),
        Some(DetailView::Request) => render_request_detail(
            frame,
            root[1],
            state,
            app.recent_selected.row().unwrap_or(state.recent.len()),
        ),
        None => render_sessions(
            frame,
            root[1],
            &state.sessions,
            app.selected.view(),
            app.focus == FocusPane::Sessions,
        ),
    }
    render_active(frame, root[2], &state.active, app.tick);
    render_recent(
        frame,
        root[3],
        &state.recent,
        app.recent_selected.view(),
        app.focus == FocusPane::Recent,
    );
    render_events(frame, root[4], &state.recent);
    render_footer(frame, root[5], app);

    if app.show_setup {
        render_setup_overlay(frame, area, &app.setup_text);
    }
    if app.show_help {
        render_help_overlay(frame, area);
    }
    match app.phase {
        MonitorPhase::Running => {}
        MonitorPhase::ConfirmingShutdown => render_shutdown_confirmation(frame, area),
        MonitorPhase::ShuttingDown => render_shutdown_overlay(frame, area, app.tick),
    }
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    state: &MonitorState,
) {
    let uptime = state
        .started_at
        .elapsed()
        .unwrap_or_else(|_| Duration::from_secs(0));
    let text = Line::from(vec![
        Span::styled(
            " claude-code-mux",
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(&app.listen_url, Style::default().fg(BG).bg(TEAL)),
        Span::styled("  uptime ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(
            format_duration(uptime),
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  sessions ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(
            state.sessions.len().to_string(),
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  active ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(
            state.active.len().to_string(),
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(text).style(Style::default().bg(TEAL)), area);
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    let color = if focused { TEAL } else { SEPARATOR };
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if focused { TEAL } else { DIM_WHITE })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL_BG))
}

fn table_header_aligned(
    cells: impl IntoIterator<Item = (&'static str, Alignment)>,
) -> Row<'static> {
    Row::new(
        cells
            .into_iter()
            .map(|(cell, alignment)| {
                Cell::from(
                    Line::from(Span::styled(cell, Style::default().fg(TEAL))).alignment(alignment),
                )
            })
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD))
}

fn render_empty_table_state(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    message: &str,
) {
    frame.render_widget(panel(title, focused), area);
    let content = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if content.width == 0 || content.height == 0 {
        return;
    }

    let line = Rect {
        y: content.y + content.height.saturating_sub(1) / 2,
        height: 1,
        ..content
    };
    frame.render_widget(
        Paragraph::new(ellipsize(message, line.width.into()))
            .alignment(Alignment::Center)
            .style(Style::default().fg(DIM).bg(PANEL_BG)),
        line,
    );
}

fn muted_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM)))
}

fn text_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
}

fn model_cell(value: Option<&str>, width: usize) -> Cell<'static> {
    text_cell(ellipsize(value.unwrap_or("-"), width))
}

/// The model a request row names.
///
/// The effective model is the only one a provider was seen putting on the
/// wire. A request the proxy answered itself ran no model at all, and one
/// whose outgoing body was never observed says only what it asked for or what
/// it was routed to; neither may read as a call of that model. The mark goes
/// in front so a narrow column keeps it when the id is cut.
fn executed_model_label(
    provider: Option<&str>,
    requested: Option<&str>,
    effective: Option<&str>,
    routed: Option<&str>,
) -> String {
    if provider == Some(LOCAL_PROVIDER) {
        return LOCAL_ANSWER_LABEL.to_string();
    }
    if let Some(model) = effective {
        return model.to_string();
    }
    // The routed value is a display projection that pairs the routed model
    // with the effective one once both are known. Nothing was observed here,
    // so keep only the routed half rather than showing a pair.
    let routed = routed.map(|routed| {
        routed
            .split_once(" → ")
            .map_or(routed, |(routed, _)| routed)
    });
    match requested.or(routed) {
        Some(model) => format!("{REQUESTED_MODEL_MARK}{model}"),
        None => "-".to_string(),
    }
}

fn executed_model_cell(
    provider: Option<&str>,
    requested: Option<&str>,
    effective: Option<&str>,
    routed: Option<&str>,
    width: usize,
) -> Cell<'static> {
    text_cell(ellipsize(
        &executed_model_label(provider, requested, effective, routed),
        width,
    ))
}

/// The same for the single column the narrowest tiers carry, which pairs the
/// provider with the model. A local answer names no model to pair it with.
fn executed_target_cell(
    provider: Option<&str>,
    requested: Option<&str>,
    effective: Option<&str>,
    routed: Option<&str>,
    width: usize,
) -> Cell<'static> {
    let label = executed_model_label(provider, requested, effective, routed);
    if provider == Some(LOCAL_PROVIDER) {
        return text_cell(ellipsize(&label, width));
    }
    target_cell(provider, Some(label.as_str()), width)
}

fn table_column_width(area: Rect, widths: &[Constraint], column: usize) -> usize {
    let table_width = area.width.saturating_sub(2);
    Layout::horizontal(widths.to_vec())
        .spacing(1)
        .split(Rect::new(0, 0, table_width, 1))
        .get(column)
        .map_or(0, |rect| usize::from(rect.width))
}

fn ellipsize(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn display_session_id(session_id: Option<&str>) -> &str {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return "no-session";
    };
    if uuid::Uuid::parse_str(session_id).is_ok() {
        return session_id
            .split_once('-')
            .map_or(session_id, |(first, _)| first);
    }
    session_id
}

/// A conversation is labelled by the agent id Claude Code assigned it, which
/// is as long as a session id is before [`display_session_id`] shortens it. Cut
/// it the same way so a nested row still fits its column, and keep the `/side`
/// suffix, which is what distinguishes a conversation from its own side calls.
fn display_conversation_label(conversation: &str) -> String {
    let (base, suffix) = match conversation.strip_suffix(SIDE_CONVERSATION_SUFFIX) {
        Some(base) => (base, SIDE_CONVERSATION_SUFFIX),
        None => (conversation, ""),
    };
    let shortened = match base.split_once('-') {
        Some((first, _)) if uuid::Uuid::parse_str(base).is_ok() => first,
        _ => &base[..base.len().min(SHORT_CONVERSATION_LEN)],
    };
    format!("{shortened}{suffix}")
}

/// How much of an agent id identifies it on screen. Claude Code's are 17 hex
/// characters; the leading eight match what a shortened session id shows.
const SHORT_CONVERSATION_LEN: usize = 8;

fn number_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
            .alignment(Alignment::Right),
    )
}

fn status_cell(value: &str) -> Cell<'static> {
    Cell::from(Span::styled(value.to_string(), status_style(value)))
}

fn status_style(value: &str) -> Style {
    Style::default().fg(status_color(value))
}

fn status_color(value: &str) -> Color {
    match value {
        "completed" => GREEN,
        "streaming" => TEAL,
        "compacting" => PURPLE,
        "failed" => RED,
        "upstream" => BLUE,
        "selected" | "started" => YELLOW,
        _ => DIM_WHITE,
    }
}

fn http_status_style(status: Option<u16>) -> Style {
    Style::default().fg(http_status_color(status))
}

fn http_status_color(status: Option<u16>) -> Color {
    match status {
        Some(200..=299) => GREEN,
        Some(400..=499) => YELLOW,
        Some(500..=599) => RED,
        Some(_) => DIM_WHITE,
        None => DIM,
    }
}

fn hit_label(ratio: Option<f64>) -> String {
    ratio
        .map(|ratio| format!("{:.0}%", ratio * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

fn hit_color(ratio: Option<f64>) -> Color {
    match ratio {
        None => DIM,
        Some(ratio) if ratio >= 0.8 => GREEN,
        Some(ratio) if ratio >= 0.5 => YELLOW,
        Some(_) => RED,
    }
}

fn hit_cell(ratio: Option<f64>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(
            hit_label(ratio),
            Style::default().fg(hit_color(ratio)),
        ))
        .alignment(Alignment::Right),
    )
}

/// Missed tokens of one request, red when there was a miss.
fn request_miss_cell(request: &CompletedRequest) -> Cell<'static> {
    let (value, color) = match request.cache.miss {
        Some(miss) => (compact_tokens(miss.missed_tokens), RED),
        None => ("-".to_string(), DIM),
    };
    Cell::from(
        Line::from(Span::styled(value, Style::default().fg(color))).alignment(Alignment::Right),
    )
}

/// Miss count and missed tokens of a session or one of its conversations, as
/// `count/tokens`.
fn cache_miss_cell(cache: &SessionCacheStats) -> Cell<'static> {
    let (value, color) = if cache.miss_count == 0 {
        ("-".to_string(), DIM)
    } else {
        (
            format!(
                "{}/{}",
                cache.miss_count,
                compact_tokens(cache.missed_tokens)
            ),
            RED,
        )
    };
    Cell::from(
        Line::from(Span::styled(value, Style::default().fg(color))).alignment(Alignment::Right),
    )
}

/// Why a cache miss happened: the cause and the idle gap against the cache
/// lifetime.
fn cache_miss_cause(miss: &crate::monitor::CacheMiss) -> String {
    let ttl = match miss.ttl {
        Some(ttl) if miss.cause == crate::monitor::CacheMissCause::Expired => {
            format!(" > ttl {}", format_duration(ttl))
        }
        Some(ttl) => format!(" (ttl {})", format_duration(ttl)),
        None => String::new(),
    };
    format!(
        "{} · idle {}{}",
        miss.cause.label(),
        format_duration(miss.gap),
        ttl
    )
}

/// One line describing a cache miss: how much, why, and the idle gap.
fn cache_miss_summary(miss: &crate::monitor::CacheMiss) -> String {
    format!(
        "{} of {} · {}",
        compact_tokens(miss.missed_tokens),
        compact_tokens(miss.expected_tokens),
        cache_miss_cause(miss)
    )
}

/// How long the main conversation's cached prefix should stay readable, going
/// by its provider's cache lifetime.
fn context_cache_expiry_label(
    stats: &crate::monitor::SessionCacheStats,
    now: SystemTime,
) -> String {
    let Some(ttl) = stats.context_ttl else {
        return String::new();
    };
    match stats.context_cache_expiry(now) {
        Some(crate::monitor::CacheExpiry::WarmFor(left)) => format!(
            " · cache warm {} more (ttl {})",
            format_duration(left),
            format_duration(ttl)
        ),
        Some(crate::monitor::CacheExpiry::ExpiredAgo(ago)) => format!(
            " · cache expired {} ago (ttl {})",
            format_duration(ago),
            format_duration(ttl)
        ),
        None => String::new(),
    }
}

/// The recent table already shows the missed tokens in its own column, so
/// the details cell carries only the cause.
fn recent_details_cell(request: &CompletedRequest) -> Cell<'static> {
    if let Some(error) = request.error.as_deref().filter(|error| !error.is_empty()) {
        return detail_cell(error);
    }
    match request.cache.miss {
        Some(miss) => Cell::from(Span::styled(
            format!("cache miss · {}", cache_miss_cause(&miss)),
            Style::default().fg(RED),
        )),
        None => detail_cell(""),
    }
}

fn rate_cell(value: String) -> Cell<'static> {
    let color = if value.contains("tok/s") {
        TEAL
    } else if value == "-" {
        DIM
    } else {
        DIM_WHITE
    };
    Cell::from(
        Line::from(Span::styled(value, Style::default().fg(color))).alignment(Alignment::Right),
    )
}

fn provider_cell(value: Option<&str>) -> Cell<'static> {
    let value = value.unwrap_or("-");
    let color = match value {
        "codex" => TEAL,
        "kimi" => Color::Rgb(190, 150, 220),
        "cursor" => Color::Rgb(140, 170, 230),
        "-" => DIM,
        _ => DIM_WHITE,
    };
    Cell::from(Span::styled(value.to_string(), Style::default().fg(color)))
}

fn detail_cell(value: &str) -> Cell<'static> {
    if value.is_empty() || value == "-" {
        Cell::from(Span::styled("", Style::default().fg(DIM)))
    } else {
        Cell::from(Span::styled(value.to_string(), Style::default().fg(YELLOW)))
    }
}

fn error_indicator(request: &CompletedRequest) -> &'static str {
    if request.status == crate::monitor::RequestStatus::Failed
        || request.http_status.is_some_and(|status| status >= 400)
        || request
            .error
            .as_deref()
            .is_some_and(|error| !error.is_empty())
    {
        "!"
    } else {
        ""
    }
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn token_value(value: Option<u64>) -> String {
    value.map(compact_tokens).unwrap_or_else(|| "-".to_string())
}

/// One count with the evidence behind it in the text itself: `~` while only an
/// estimate has arrived, `n/a` when the backend never reported the count, the
/// plain number once it did — a reported zero included. Colour repeats the
/// mark and never carries the meaning alone.
fn quality_token_label(value: Option<u64>, quality: UsageQuality) -> String {
    match quality {
        UsageQuality::Missing => MISSING_TOKENS_LABEL.to_string(),
        UsageQuality::Opening => format!("{OPENING_TOKENS_MARK}{}", token_value(value)),
        UsageQuality::Exact => token_value(value),
    }
}

fn quality_color(quality: UsageQuality) -> Color {
    match quality {
        UsageQuality::Missing => DIM,
        UsageQuality::Opening => YELLOW,
        UsageQuality::Exact => DIM_WHITE,
    }
}

fn quality_token_cell(value: Option<u64>, quality: UsageQuality) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(
            quality_token_label(value, quality),
            Style::default().fg(quality_color(quality)),
        ))
        .alignment(Alignment::Right),
    )
}

/// The prompt size with the evidence its parts give it.
///
/// A total the backend measured itself is its own exact number. Otherwise the
/// prompt is the sum of the three input categories and is no firmer than they
/// are: one provisional part makes the sum provisional, and no part reported
/// at all leaves no prompt to state.
fn prompt_label(prompt: Option<u64>, cache: &RequestCache, quality: QualityFields) -> String {
    if cache.reported_prompt_tokens.is_some() {
        return token_value(prompt);
    }
    let parts = [quality.input, quality.cache_read, quality.cache_write];
    if parts.iter().all(|part| *part == UsageQuality::Missing) {
        return MISSING_TOKENS_LABEL.to_string();
    }
    if parts.contains(&UsageQuality::Opening) {
        return format!("{OPENING_TOKENS_MARK}{}", token_value(prompt));
    }
    token_value(prompt)
}

/// A request's four counts, each with the evidence behind it in its own text.
///
/// A total the backend measured itself is named only when the categories do
/// not already reach it: it covers the same tokens, so repeating an equal
/// number would read as a fifth one to add.
fn request_token_summary(
    prompt: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
    cache: &RequestCache,
    quality: QualityFields,
    write_quality: CacheWriteQuality,
) -> String {
    let components = [input, cache.read_tokens, cache.write_tokens]
        .into_iter()
        .flatten()
        .sum::<u64>();
    let backend_total = match cache.reported_prompt_tokens {
        Some(total) if total != components => format!(" · backend total {}", compact_tokens(total)),
        _ => String::new(),
    };
    // The legend covers every mark the request's lines carry, the buckets of
    // the cache line included.
    let all_exact = [
        quality.input,
        quality.cache_read,
        quality.cache_write,
        quality.output,
        write_quality.ephemeral_5m,
        write_quality.ephemeral_1h,
    ]
    .into_iter()
    .all(|quality| quality == UsageQuality::Exact);
    let legend = if all_exact {
        String::new()
    } else {
        format!(" · {OPENING_TOKENS_MARK} provisional · {MISSING_TOKENS_LABEL} not reported")
    };
    format!(
        "{} prompt · {} in · {} read · {} write · {} out{}{}",
        prompt_label(prompt, cache, quality),
        quality_token_label(input, quality.input),
        quality_token_label(cache.read_tokens, quality.cache_read),
        quality_token_label(cache.write_tokens, quality.cache_write),
        quality_token_label(output, quality.output),
        backend_total,
        legend
    )
}

/// The two lifetime buckets a cache write is made of, each with its own
/// evidence.
///
/// A bucket is part of the write rather than a token beside it, and the
/// backend reports it independently of the total: a bucket the response never
/// named is unknown, not the rest of the write. The caveat comes before the
/// numbers so that cutting the line short at a narrow width can only take the
/// numbers away, never turn them into a split of the write.
fn cache_write_bucket_summary(cache: &RequestCache, quality: CacheWriteQuality) -> String {
    format!(
        "write parts reported separately: 5m {}, 1h {}",
        quality_token_label(cache.write_5m_tokens, quality.ephemeral_5m),
        quality_token_label(cache.write_1h_tokens, quality.ephemeral_1h)
    )
}

/// What a request asked for and what a provider was seen putting on the wire.
///
/// Only the second says what answered. The routed model is deliberately left
/// out: the snapshot carries it as a display string that already pairs it with
/// the effective one, so naming it here would repeat the executed model.
fn request_model_summary(
    provider: Option<&str>,
    requested: Option<&str>,
    effective: Option<&str>,
) -> String {
    let executed = if provider == Some(LOCAL_PROVIDER) {
        "none (answered locally)".to_string()
    } else {
        match effective {
            Some(model) => model.to_string(),
            // The legend for the mark the rows carry: nothing on a row has
            // room to say what it means.
            None => format!("not observed (rows mark it {REQUESTED_MODEL_MARK})"),
        }
    };
    format!(
        "requested {} · executed {}",
        requested.unwrap_or("-"),
        executed
    )
}

fn spinner(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick % FRAMES.len()]
}

fn sparkline_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

fn token_sparkline(samples: &[(SystemTime, u64)], width: usize, now: SystemTime) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if width == 0 {
        return String::new();
    }

    let mut buckets = HashMap::<u64, u64>::new();
    for (timestamp, tokens) in samples {
        let bucket = sparkline_bucket(*timestamp);
        let total = buckets.entry(bucket).or_default();
        *total = total.saturating_add(*tokens);
    }

    let current_bucket = sparkline_bucket(now);
    let first_bucket = current_bucket.saturating_sub(width.saturating_sub(1) as u64);
    (first_bucket..=current_bucket)
        .map(|bucket| {
            let value = buckets.get(&bucket).copied().unwrap_or(0);
            if value == 0 {
                return ' ';
            }
            let scaled = value.min(SESSION_SPARKLINE_MAX_TOKENS);
            let level = (u128::from(scaled) * LEVELS.len() as u128)
                .div_ceil(u128::from(SESSION_SPARKLINE_MAX_TOKENS))
                .saturating_sub(1) as usize;
            LEVELS[level]
        })
        .collect()
}

fn token_sparkline_line(
    samples: &[(SystemTime, u64)],
    width: usize,
    now: SystemTime,
) -> Line<'static> {
    let mut sparkline = token_sparkline(samples, width, now);
    let current = sparkline
        .pop()
        .map_or_else(String::new, |value| value.to_string());
    Line::from(vec![
        Span::styled(sparkline, Style::default().fg(BLUE)),
        Span::styled(current, Style::default().fg(DIM)),
    ])
}

fn column_constraints<K>(columns: &[ColumnSpec<K>]) -> Vec<Constraint> {
    columns.iter().map(ColumnSpec::constraint).collect()
}

fn column_header<K>(columns: &[ColumnSpec<K>]) -> Row<'static> {
    table_header_aligned(
        columns
            .iter()
            .map(|column| (column.header, column.alignment)),
    )
}

fn target_cell(provider: Option<&str>, model: Option<&str>, width: usize) -> Cell<'static> {
    let provider = provider.unwrap_or("-");
    let model = model.unwrap_or("-");
    text_cell(ellipsize(&format!("{provider}/{model}"), width))
}

/// The Sessions table's `ID` column where the tier shows a bare id rather than
/// the tree width. Two columns wider than [`ID_WIDTH`], because a session row
/// carries the aggregate mark in front of its id and the id has to survive it;
/// every tier below the widest has that much room to spare.
const SESSION_ID_WIDTH: u16 = ID_WIDTH + 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionColumn {
    Marker,
    Id,
    Project,
    Active,
    Requests,
    Failures,
    Counts,
    Provider,
    Model,
    Target,
    Effort,
    Context,
    Hit,
    Misses,
    Input,
    Output,
    Rate,
    Activity,
    Status,
}

fn session_columns(tier: LayoutTier, show_full_sparkline: bool) -> Vec<ColumnSpec<SessionColumn>> {
    use SessionColumn as C;
    match (tier, show_full_sparkline) {
        (LayoutTier::Wide, true) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, SESSION_TREE_ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Active, "A", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Requests, "R", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Failures, "F", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Context, "Ctx", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Hit, "Hit", Alignment::Right, HIT_WIDTH),
            ColumnSpec::fixed(C::Misses, "Miss", Alignment::Right, MISS_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s · 4k", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Wide, false) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, SESSION_TREE_ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Context, "Ctx", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Hit, "Hit", Alignment::Right, HIT_WIDTH),
            ColumnSpec::fixed(C::Misses, "Miss", Alignment::Right, MISS_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Expanded, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, SESSION_ID_WIDTH),
            // The tier is sized so the sparkline header still fits at its
            // narrowest, so the two columns the aggregate mark needs come out
            // of the project rather than out of the flexible column. Ten is
            // what the narrow tier already gives a project name.
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, 10),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Medium, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, SESSION_ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Tok/10s", Alignment::Left, 8),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Narrow, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, SESSION_ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, 10),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Trend", Alignment::Left, 6),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Emergency, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, SESSION_ID_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
    }
}

/// One line of the Sessions table: a session, or one of its conversations
/// nested under it. Selection walks this flattened list, so a row's index here
/// is the index the table renders it at.
#[derive(Clone, Copy)]
enum SessionRow<'a> {
    Session(&'a SessionSummary),
    Conversation {
        /// The session the conversation belongs to: what makes its label a row
        /// identity, since another session may name an agent the same way.
        session: &'a SessionSummary,
        conversation: &'a ConversationSummary,
        /// The last conversation of its level, which closes that branch.
        last: bool,
    },
}

/// Every session followed by its conversations, in display order: the
/// depth-first walk the session summary already arranged them in.
fn session_rows(sessions: &[SessionSummary]) -> Vec<SessionRow<'_>> {
    sessions
        .iter()
        .flat_map(|session| {
            std::iter::once(SessionRow::Session(session)).chain(
                session
                    .conversations
                    .iter()
                    .enumerate()
                    .map(|(index, conversation)| SessionRow::Conversation {
                        session,
                        conversation,
                        last: is_last_child(&session.conversations, index),
                    }),
            )
        })
        .collect()
}

/// Whether a conversation closes its level: the walk moves back up, or ends,
/// before another row of the same depth appears.
fn is_last_child(conversations: &[ConversationSummary], index: usize) -> bool {
    let depth = conversations[index].depth;
    conversations[index + 1..]
        .iter()
        .find(|conversation| conversation.depth <= depth)
        .is_none_or(|conversation| conversation.depth < depth)
}

/// The session a marked row belongs to: itself, or the session a conversation
/// row hangs under. It answers for whatever row the pane is marking, a pane
/// nobody has moved in included, and the marked row is always one this
/// snapshot has — a conversation that ended already moved the mark to the
/// session it hung under.
fn selected_session_index(sessions: &[SessionSummary], selected: usize) -> Option<usize> {
    let mut row = 0;
    for (index, session) in sessions.iter().enumerate() {
        row += 1 + session.conversations.len();
        if selected < row {
            return Some(index);
        }
    }
    None
}

/// What a conversation row carries in front of its label for a parent the
/// session never saw. The order and the depth are the walk's; this only says
/// why a row the walk could not place sits where a root does.
fn unknown_parent_mark(conversation: &ConversationSummary) -> &'static str {
    match conversation.parent.is_none() && conversation.raw_parent.is_some() {
        true => UNKNOWN_PARENT_MARK,
        false => "",
    }
}

/// Whether a conversation row stands for requests the proxy answered itself,
/// which ran no model and so have none for the row to name. The record may
/// carry one all the same — the id the request was routed to is noted before
/// anything decides to answer it here — and naming it would read as a call that
/// never happened, exactly as it would on a request row.
fn is_local_answer(conversation: &ConversationSummary) -> bool {
    conversation.provider.as_deref() == Some(LOCAL_PROVIDER)
}

/// What a session row can honestly say about the model behind its figures.
///
/// The per-model rollups are keyed on the model a producer was seen putting on
/// the wire, so one such row is one answer and several have no single one. The
/// last model the session used is not that answer: it names one request, and
/// the cell speaks for all of them. Rows the proxy answered itself ran no
/// model, so they are counted apart from the ones that did — exactly as
/// [`session_provider_label`] counts backends — and never make the one model
/// that ran look like two.
enum SessionModel {
    /// Nothing has been counted for this session yet.
    Unknown,
    /// Every request the proxy answered itself. No model ran.
    LocalOnly,
    /// One model ran everything the session counted.
    One(String),
    /// Several did, and none of them is the session's model. The count is of
    /// the models that ran, local answers excluded.
    Several(usize),
}

impl SessionModel {
    fn of(models: &[ModelUsage]) -> Self {
        if models.is_empty() {
            return Self::Unknown;
        }
        let executed = models
            .iter()
            .filter(|row| row.provider.as_deref() != Some(LOCAL_PROVIDER))
            .collect::<Vec<_>>();
        match executed.as_slice() {
            [] => Self::LocalOnly,
            [row] => Self::One(model_usage_label(row)),
            rows => Self::Several(rows.len()),
        }
    }

    /// The `Model` cell.
    fn label(&self) -> String {
        match self {
            Self::Unknown => "-".to_string(),
            Self::LocalOnly => LOCAL_ANSWER_LABEL.to_string(),
            Self::One(model) => model.clone(),
            Self::Several(count) => format!("{MIXED_MODELS_LABEL} {count}"),
        }
    }

    /// The single column the narrowest tiers carry, which pairs a backend with
    /// a model. Only a session that ran exactly one model has a pair to name;
    /// the others would be claiming a backend for requests that never went to
    /// it, so they say what the model cell says and nothing more.
    fn target_label(&self, provider: Option<&str>) -> String {
        match self {
            Self::One(model) => format!("{}/{model}", provider.unwrap_or("-")),
            other => other.label(),
        }
    }
}

/// The backend behind a session row's figures, on the same terms as its model.
///
/// The rollups name every backend the session's requests actually went to, so
/// the last of them is no more the session's than the last model is. Answers
/// the proxy gave itself went to no backend at all: they never make the one
/// backend that did run ambiguous, and where they are all there is, that is
/// what the cell says.
fn session_provider_label(models: &[ModelUsage]) -> String {
    let mut backends: Vec<&str> = Vec::new();
    let mut answered_locally = false;
    for row in models {
        match row.provider.as_deref() {
            Some(LOCAL_PROVIDER) => answered_locally = true,
            Some(backend) if !backends.contains(&backend) => backends.push(backend),
            _ => {}
        }
    }
    match backends.as_slice() {
        [] if answered_locally => LOCAL_PROVIDER.to_string(),
        [] => "-".to_string(),
        [backend] => (*backend).to_string(),
        several => format!("{MIXED_MODELS_LABEL} {}", several.len()),
    }
}

/// One rollup row's model, named the way a request row names its own. The
/// routed model is a per-request display projection with no rollup of its own,
/// so a row has none to fall back on.
fn model_usage_label(row: &ModelUsage) -> String {
    executed_model_label(
        row.provider.as_deref(),
        sole_requested_model(row),
        row.model.as_deref(),
        None,
    )
}

/// The caller model behind a rollup row when there is only one of them. Where
/// several callers reached the same model, none of them is the one a cell cut
/// to a single name may claim.
fn sole_requested_model(row: &ModelUsage) -> Option<&str> {
    match row.requested_models.as_slice() {
        [(requested, _)] => requested.as_deref(),
        _ => None,
    }
}

impl SessionRow<'_> {
    /// What names this row across snapshots, which is not where it sits: rows
    /// arrive above it and below it between one snapshot and the next.
    fn key(&self) -> SessionRowKey {
        match self {
            Self::Session(session) => SessionRowKey {
                session: session.session_id.clone(),
                conversation: None,
            },
            Self::Conversation {
                session,
                conversation,
                ..
            } => SessionRowKey {
                session: session.session_id.clone(),
                conversation: Some(conversation.conversation.clone()),
            },
        }
    }

    /// Whether this row is the one the key names. Comparing without building
    /// the key keeps a re-render off the allocator: the pane asks this of
    /// every row until one answers, once a frame.
    fn matches(&self, key: &SessionRowKey) -> bool {
        match self {
            Self::Session(session) => {
                key.conversation.is_none() && session.session_id == key.session
            }
            Self::Conversation {
                session,
                conversation,
                ..
            } => {
                session.session_id == key.session
                    && key.conversation.as_deref() == Some(conversation.conversation.as_str())
            }
        }
    }

    /// The `ID` cell: the session as a whole behind the aggregate mark, or one
    /// of its conversations behind a tree glyph indented one step per level
    /// below the session row.
    fn id_label(&self, width: usize) -> String {
        match self {
            Self::Session(session) => format!(
                "{SESSION_AGGREGATE_MARK}{}",
                ellipsize(
                    display_session_id(session.session_id.as_deref()),
                    width.saturating_sub(SESSION_AGGREGATE_MARK.chars().count()),
                )
            ),
            Self::Conversation {
                conversation, last, ..
            } => {
                let branch = format!(
                    "{}{}{}",
                    " ".repeat(conversation.depth),
                    if *last { "└─" } else { "├─" },
                    unknown_parent_mark(conversation),
                );
                let short = display_conversation_label(&conversation.conversation);
                // The main thread is the one conversation Claude Code names no
                // agent for. Spell it out where the cell has room, so the row
                // reads as the session's own thread rather than as a lane whose
                // id went missing.
                let base = conversation
                    .conversation
                    .strip_suffix(SIDE_CONVERSATION_SUFFIX)
                    .unwrap_or(&conversation.conversation);
                let label = (base == MAIN_CONVERSATION)
                    .then(|| {
                        format!(
                            "{MAIN_CONVERSATION_LABEL}{}",
                            &conversation.conversation[base.len()..]
                        )
                    })
                    .filter(|label| branch.chars().count() + label.chars().count() <= width)
                    .unwrap_or(short);
                // The id is what a narrow column gives up: the glyphs in front
                // of it are what says where the row hangs, which is the column
                // the tree is drawn in.
                let room = width.saturating_sub(branch.chars().count());
                ellipsize(&format!("{branch}{}", ellipsize(&label, room)), width)
            }
        }
    }

    /// Columns that describe the session as a whole stay on its own row.
    fn project(&self) -> &str {
        match self {
            Self::Session(session) => session.project.as_deref().unwrap_or("-"),
            Self::Conversation { .. } => "",
        }
    }

    fn effort(&self) -> &str {
        match self {
            Self::Session(session) => session.effort.as_deref().unwrap_or("-"),
            Self::Conversation { .. } => "",
        }
    }

    fn rate_label(&self) -> String {
        match self {
            Self::Session(session) => session.rate().label(),
            Self::Conversation { .. } => String::new(),
        }
    }

    fn output_token_samples(&self) -> &[(SystemTime, u64)] {
        match self {
            Self::Session(session) => &session.output_token_samples,
            Self::Conversation { .. } => &[],
        }
    }

    fn counts(&self) -> (usize, usize, usize) {
        match self {
            Self::Session(session) => (
                session.active_count,
                session.request_count,
                session.failure_count,
            ),
            Self::Conversation { conversation, .. } => (
                conversation.active_count,
                conversation.request_count,
                conversation.failure_count,
            ),
        }
    }

    /// The `Provider` cell.
    ///
    /// A conversation row names the backend of the last request it saw. A
    /// session row stands for every request of the session, so it counts the
    /// backends its rollups name rather than letting the newest one speak for
    /// all of them.
    fn provider_label(&self) -> String {
        match self {
            Self::Session(session) => session_provider_label(&session.models),
            Self::Conversation { conversation, .. } => {
                conversation.provider.as_deref().unwrap_or("-").to_string()
            }
        }
    }

    /// The `Model` cell.
    ///
    /// A conversation row names the model of the last request it saw. A
    /// session row stands for every request of the session, which may have run
    /// on several; naming one of those would read as the model all its figures
    /// belong to, so it says how many there were and leaves the breakdown to
    /// the detail pane. A lane the proxy answered itself reads as the answer it
    /// was, the way the request row of the same request does, whatever model
    /// the record happens to carry: none of them ran.
    fn model_label(&self) -> String {
        match self {
            Self::Session(session) => SessionModel::of(&session.models).label(),
            Self::Conversation { conversation, .. } if is_local_answer(conversation) => {
                LOCAL_ANSWER_LABEL.to_string()
            }
            Self::Conversation { conversation, .. } => {
                conversation.model.as_deref().unwrap_or("-").to_string()
            }
        }
    }

    /// The `Target` cell, which pairs the backend with the model cell.
    fn target_label(&self) -> String {
        match self {
            Self::Session(session) => SessionModel::of(&session.models)
                .target_label(Some(session_provider_label(&session.models).as_str())),
            // No model ran, so there is no backend to pair one with.
            Self::Conversation { conversation, .. } if is_local_answer(conversation) => {
                LOCAL_ANSWER_LABEL.to_string()
            }
            Self::Conversation { conversation, .. } => format!(
                "{}/{}",
                conversation.provider.as_deref().unwrap_or("-"),
                conversation.model.as_deref().unwrap_or("-")
            ),
        }
    }

    fn cache(&self) -> &SessionCacheStats {
        match self {
            Self::Session(session) => &session.cache,
            Self::Conversation { conversation, .. } => &conversation.cache,
        }
    }

    fn cache_hit_ratio(&self) -> Option<f64> {
        match self {
            Self::Session(session) => session.cache_hit_ratio(),
            Self::Conversation { conversation, .. } => conversation.cache_hit_ratio(),
        }
    }

    fn input_tokens(&self) -> u64 {
        match self {
            Self::Session(session) => session.input_tokens,
            Self::Conversation { conversation, .. } => conversation.input_tokens,
        }
    }

    fn output_tokens(&self) -> u64 {
        match self {
            Self::Session(session) => session.output_tokens,
            Self::Conversation { conversation, .. } => conversation.output_tokens,
        }
    }

    fn last_status(&self) -> &str {
        match self {
            Self::Session(session) => &session.last_status,
            Self::Conversation { conversation, .. } => &conversation.last_status,
        }
    }
}

fn render_sessions(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    sessions: &[SessionSummary],
    selection: SelectionView,
    focused: bool,
) {
    if sessions.is_empty() {
        // A pane with nothing left to mark still says what became of the row
        // that was.
        render_empty_table_state(
            frame,
            area,
            &selection.title("Sessions"),
            focused,
            "No sessions",
        );
        return;
    }

    let tier = LayoutTier::for_outer_width(area.width);
    let show_full_sparkline = tier == LayoutTier::Wide && area.width >= SESSION_SPARKLINE_MIN_WIDTH;
    let columns = session_columns(tier, show_full_sparkline);
    let widths = column_constraints(&columns);
    let now = SystemTime::now();
    let rows = session_rows(sessions)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let (active_count, request_count, failure_count) = row.counts();
            let cells = columns
                .iter()
                .enumerate()
                .map(|(column_index, column)| {
                    let width = table_column_width(area, &widths, column_index);
                    match column.key {
                        SessionColumn::Marker => {
                            let marker = if focused && selection.marks(index) {
                                ">"
                            } else {
                                " "
                            };
                            Cell::from(Span::styled(marker, Style::default().fg(TEAL)))
                        }
                        SessionColumn::Id => text_cell(row.id_label(width)),
                        SessionColumn::Project => text_cell(ellipsize(row.project(), width)),
                        SessionColumn::Active => number_cell(active_count.to_string()),
                        SessionColumn::Requests => number_cell(request_count.to_string()),
                        SessionColumn::Failures => number_cell(failure_count.to_string()),
                        SessionColumn::Counts => {
                            number_cell(format!("{active_count}/{request_count}/{failure_count}"))
                        }
                        SessionColumn::Provider => {
                            provider_cell(Some(row.provider_label().as_str()))
                        }
                        SessionColumn::Model => model_cell(Some(row.model_label().as_str()), width),
                        SessionColumn::Target => text_cell(ellipsize(&row.target_label(), width)),
                        SessionColumn::Effort => text_cell(row.effort()),
                        SessionColumn::Context => number_cell(if row.cache().context_tokens == 0 {
                            "-".to_string()
                        } else {
                            compact_tokens(row.cache().context_tokens)
                        }),
                        SessionColumn::Hit => hit_cell(row.cache_hit_ratio()),
                        SessionColumn::Misses => cache_miss_cell(row.cache()),
                        SessionColumn::Input => number_cell(compact_tokens(row.input_tokens())),
                        SessionColumn::Output => number_cell(compact_tokens(row.output_tokens())),
                        SessionColumn::Rate => rate_cell(row.rate_label()),
                        SessionColumn::Activity => {
                            Cell::from(token_sparkline_line(row.output_token_samples(), width, now))
                        }
                        SessionColumn::Status => status_cell(row.last_status()),
                    }
                })
                .collect::<Vec<_>>();
            Row::new(cells).style(if selection.marks(index) {
                Style::default().bg(SELECTED_BG)
            } else {
                Style::default().bg(PANEL_BG)
            })
        })
        .collect::<Vec<_>>();
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel(&selection.title("Sessions"), focused));
    let mut table_state = TableState::default().with_selected(selection.row);
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveColumn {
    Started,
    Status,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Input,
    Output,
    Rate,
    Elapsed,
}

fn active_columns(tier: LayoutTier) -> Vec<ColumnSpec<ActiveColumn>> {
    use ActiveColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::flex(C::Endpoint, "Endpoint", Alignment::Left, 1),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
    }
}

fn render_active(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    active: &[ActiveRequest],
    tick: usize,
) {
    if active.is_empty() {
        render_empty_table_state(frame, area, "Active requests", false, "No active requests");
        return;
    }

    let columns = active_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = active.iter().map(|request| {
        let status = if request.status == crate::monitor::RequestStatus::Compacting {
            format!("{}{}", spinner(tick), request.status.label())
        } else {
            format!("{} {}", spinner(tick), request.status.label())
        };
        let quality = request.usage_quality();
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    ActiveColumn::Started => muted_cell(format_system_time(request.started_at)),
                    ActiveColumn::Status => Cell::from(Span::styled(
                        status.clone(),
                        status_style(request.status.label()),
                    )),
                    ActiveColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    ActiveColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    ActiveColumn::Provider => provider_cell(request.provider.as_deref()),
                    ActiveColumn::Model => executed_model_cell(
                        request.provider.as_deref(),
                        request.requested_model.as_deref(),
                        request.effective_model.as_deref(),
                        request.model.as_deref(),
                        width,
                    ),
                    ActiveColumn::Target => executed_target_cell(
                        request.provider.as_deref(),
                        request.requested_model.as_deref(),
                        request.effective_model.as_deref(),
                        request.model.as_deref(),
                        width,
                    ),
                    ActiveColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    ActiveColumn::Endpoint => muted_cell(request.endpoint.label()),
                    ActiveColumn::Input => quality_token_cell(request.input_tokens, quality.input),
                    ActiveColumn::Output => {
                        quality_token_cell(request.output_tokens, quality.output)
                    }
                    ActiveColumn::Rate => rate_cell(request.rate().label()),
                    ActiveColumn::Elapsed => number_cell(format_duration(request.elapsed())),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Active requests", false));
    frame.render_widget(table, area);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecentColumn {
    Finished,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Latency,
    Rate,
    Hit,
    Miss,
    Input,
    Output,
    Details,
    Error,
}

fn recent_columns(tier: LayoutTier) -> Vec<ColumnSpec<RecentColumn>> {
    use RecentColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Hit, "Hit", Alignment::Right, HIT_WIDTH),
            ColumnSpec::fixed(C::Miss, "Miss", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::flex(C::Details, "Details", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Hit, "Hit", Alignment::Right, HIT_WIDTH),
            ColumnSpec::fixed(C::Miss, "Miss", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Hit, "Hit", Alignment::Right, HIT_WIDTH),
            ColumnSpec::fixed(C::Miss, "Miss", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Hit, "Hit", Alignment::Right, HIT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
    }
}

fn http_code_cell(status: Option<u16>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "-".to_string()),
            http_status_style(status),
        ))
        .alignment(Alignment::Right),
    )
}

fn render_recent(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    recent: &[CompletedRequest],
    selection: SelectionView,
    focused: bool,
) {
    if recent.is_empty() {
        render_empty_table_state(
            frame,
            area,
            &selection.title("Recent requests"),
            focused,
            "No recent requests",
        );
        return;
    }

    let columns = recent_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = recent.iter().enumerate().map(|(index, request)| {
        let quality = request.usage_quality();
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    RecentColumn::Finished => muted_cell(format_system_time(request.finished_at)),
                    RecentColumn::Code => http_code_cell(request.http_status),
                    RecentColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    RecentColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    RecentColumn::Provider => provider_cell(request.provider.as_deref()),
                    RecentColumn::Model => executed_model_cell(
                        request.provider.as_deref(),
                        request.requested_model.as_deref(),
                        request.effective_model.as_deref(),
                        request.model.as_deref(),
                        width,
                    ),
                    RecentColumn::Target => executed_target_cell(
                        request.provider.as_deref(),
                        request.requested_model.as_deref(),
                        request.effective_model.as_deref(),
                        request.model.as_deref(),
                        width,
                    ),
                    RecentColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    RecentColumn::Endpoint => muted_cell(request.endpoint.label()),
                    RecentColumn::Latency => number_cell(format_duration(request.latency)),
                    RecentColumn::Rate => rate_cell(request.rate().label()),
                    RecentColumn::Hit => hit_cell(request.cache_hit_ratio()),
                    RecentColumn::Miss => request_miss_cell(request),
                    RecentColumn::Input => quality_token_cell(request.input_tokens, quality.input),
                    RecentColumn::Output => {
                        quality_token_cell(request.output_tokens, quality.output)
                    }
                    RecentColumn::Details => recent_details_cell(request),
                    RecentColumn::Error => detail_cell(error_indicator(request)),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if focused && selection.marks(index) {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel(&selection.title("Recent requests"), focused));
    let mut table_state = TableState::default().with_selected(selection.row);
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventColumn {
    Time,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Message,
}

fn event_columns(tier: LayoutTier) -> Vec<ColumnSpec<EventColumn>> {
    use EventColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
    }
}

fn render_events(frame: &mut ratatui::Frame<'_>, area: Rect, recent: &[CompletedRequest]) {
    let events = recent
        .iter()
        .filter(|request| {
            request.status == crate::monitor::RequestStatus::Failed
                || request.http_status.is_some_and(|status| status >= 400)
                || request.error.is_some()
        })
        .take(12)
        .collect::<Vec<_>>();
    if events.is_empty() {
        render_empty_table_state(frame, area, "Events", false, "No events");
        return;
    }

    let columns = event_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = events.iter().map(|request| {
        let message = request
            .error
            .as_deref()
            .filter(|error| !error.is_empty())
            .unwrap_or("-");
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    EventColumn::Time => muted_cell(format_system_time(request.finished_at)),
                    EventColumn::Code => http_code_cell(request.http_status),
                    EventColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    EventColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    EventColumn::Provider => provider_cell(request.provider.as_deref()),
                    EventColumn::Model => executed_model_cell(
                        request.provider.as_deref(),
                        request.requested_model.as_deref(),
                        request.effective_model.as_deref(),
                        request.model.as_deref(),
                        width,
                    ),
                    EventColumn::Message => detail_cell(message),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Events", false));
    frame.render_widget(table, area);
}

/// The evidence behind one accumulated count, stated the way a single
/// request's is.
///
/// A total is no firmer than its weakest part: one request still holding an
/// estimate, or one that never reported the count at all, leaves the sum open
/// to being replaced. Only a count no request behind it ever reported is
/// missing outright, and a row nothing was metered for has no evidence at all.
fn coverage_quality(coverage: QualityCoverage) -> UsageQuality {
    let requests = coverage.requests();
    if requests == 0 || coverage.missing == requests {
        return UsageQuality::Missing;
    }
    if coverage.is_exact() {
        return UsageQuality::Exact;
    }
    UsageQuality::Opening
}

/// One row's four counts, each with the evidence behind it in its own text.
///
/// A row nothing was metered for says so in words: a request the proxy
/// answered itself, or a token estimate, spends nothing a backend counts, and
/// four unreported counts in a line would read as four failures to report.
fn rollup_token_summary(
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    evidence: &UsageEvidence,
) -> String {
    let categories = [
        (input, evidence.input),
        (cache_read, evidence.cache_read),
        (cache_write, evidence.cache_write),
        (output, evidence.output),
    ];
    if categories
        .iter()
        .all(|(_, coverage)| coverage.requests() == 0)
    {
        return UNCOUNTED_TOKENS_LABEL.to_string();
    }
    let [input, cache_read, cache_write, output] = categories
        .map(|(value, coverage)| quality_token_label(Some(value), coverage_quality(coverage)));
    format!("{input} in · {cache_read} read · {cache_write} write · {output} out")
}

/// The caller models behind one rollup row, most asked for first.
///
/// The row is keyed on the model that ran, not on the one a caller typed, so
/// several names can share it. The list is cut where the line stops being
/// readable and says how many it left out rather than trailing off. A row with
/// no callers recorded at all contributes nothing rather than an empty list.
fn requested_models_label(requested: &[(Option<String>, usize)]) -> String {
    if requested.is_empty() {
        return String::new();
    }
    let mut entries = requested.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let listed = entries
        .iter()
        .take(REQUESTED_MODEL_LIMIT)
        .map(|(model, count)| {
            format!(
                "{}×{count}",
                model.as_deref().unwrap_or(UNNAMED_MODEL_LABEL)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    match entries.len().saturating_sub(REQUESTED_MODEL_LIMIT) {
        0 => format!("asked: {listed}"),
        more => format!("asked: {listed}, +{more} more"),
    }
}

/// What one backend and one model that ran cost the session, on one line.
fn model_rollup_line(row: &ModelUsage) -> String {
    let model = model_usage_label(row);
    let target = if row.provider.as_deref() == Some(LOCAL_PROVIDER) {
        // No backend ran it, so there is none to pair the model with.
        model
    } else {
        format!("{}/{model}", row.provider.as_deref().unwrap_or("-"))
    };
    let mut parts = vec![target];
    let asked = requested_models_label(&row.requested_models);
    if !asked.is_empty() {
        parts.push(asked);
    }
    parts.push(rollup_token_summary(
        row.input_tokens,
        row.cache_read_tokens,
        row.cache_write_tokens,
        row.output_tokens,
        &row.evidence,
    ));
    parts.push(format!("{} req", row.request_count));
    if row.failure_count > 0 {
        parts.push(format!("{} err", row.failure_count));
    }
    parts.join(" · ")
}

/// What the requests that named no conversation add up to.
///
/// They are counted in their own right. The conversation rows are not parts of
/// the session row, so this is not what is left of it once they are taken out.
fn unattributed_line(unattributed: &UnattributedUsage) -> String {
    let mut line = format!("{} req", unattributed.request_count);
    if unattributed.failure_count > 0 {
        line.push_str(&format!(" · {} err", unattributed.failure_count));
    }
    line.push_str(" · ");
    line.push_str(&rollup_token_summary(
        unattributed.input_tokens,
        unattributed.cache_read_tokens,
        unattributed.cache_write_tokens,
        unattributed.output_tokens,
        &unattributed.evidence,
    ));
    line
}

/// How many requests behind each of a row's four counts reported it, held only
/// an estimate, or never reported it at all. A total made of any of the last
/// two is not the number it looks like, and this is where that is said in
/// figures rather than left to a mark on one cell.
fn evidence_line(evidence: &UsageEvidence) -> String {
    let counts = |coverage: QualityCoverage| {
        format!(
            "{}/{}/{}",
            coverage.exact, coverage.opening, coverage.missing
        )
    };
    format!(
        "in {} · read {} · write {} · out {} · req exact/{OPENING_TOKENS_MARK}/{MISSING_TOKENS_LABEL}",
        counts(evidence.input),
        counts(evidence.cache_read),
        counts(evidence.cache_write),
        counts(evidence.output),
    )
}

fn evidence_color(evidence: &UsageEvidence) -> Color {
    let exact = [
        evidence.input,
        evidence.cache_read,
        evidence.cache_write,
        evidence.output,
    ]
    .into_iter()
    .all(|coverage| coverage.is_exact());
    if exact { DIM_WHITE } else { YELLOW }
}

fn render_session_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorState,
    selected: usize,
) {
    let lines = if let Some(session) = state.sessions.get(selected) {
        let mut lines = vec![
            detail_line("session", session.label(), WHITE),
            detail_line("project", session.project.as_deref().unwrap_or("-"), TEAL),
            detail_line("active requests", session.active_count.to_string(), YELLOW),
            detail_line(
                "total requests",
                session.request_count.to_string(),
                DIM_WHITE,
            ),
            detail_line("failures", session.failure_count.to_string(), RED),
            // These three describe the newest request of the session, not all
            // of them, and are named for it: the rollups below count every
            // backend and model the session used, and an unqualified "model"
            // beside them would read as a claim about the whole session.
            detail_line(
                "last provider",
                session.provider.as_deref().unwrap_or("-"),
                TEAL,
            ),
            detail_line(
                "last model",
                session.model.as_deref().unwrap_or("-"),
                DIM_WHITE,
            ),
            detail_line("effort", session.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line(
                "tokens",
                rollup_token_summary(
                    session.input_tokens,
                    session.cache_read_tokens,
                    session.cache_write_tokens,
                    session.output_tokens,
                    &session.evidence,
                ),
                DIM_WHITE,
            ),
        ];
        // What the session cost per backend and per model that ran. The rows
        // are the same requests the totals above are made of, counted another
        // way, so they are listed rather than summed under them.
        for (index, row) in session
            .models
            .iter()
            .take(SESSION_MODEL_ROLLUP_LIMIT)
            .enumerate()
        {
            let label = if index == 0 { "models" } else { "" };
            lines.push(detail_line(label, model_rollup_line(row), DIM_WHITE));
        }
        if let Some(hidden) = session
            .models
            .len()
            .checked_sub(SESSION_MODEL_ROLLUP_LIMIT)
            .filter(|hidden| *hidden > 0)
        {
            lines.push(detail_line("", format!("+{hidden} more"), DIM));
        }
        if session.unattributed.request_count > 0 {
            lines.push(detail_line(
                "unattributed",
                unattributed_line(&session.unattributed),
                DIM_WHITE,
            ));
        }
        lines.push(detail_line(
            "evidence",
            evidence_line(&session.evidence),
            evidence_color(&session.evidence),
        ));
        lines.extend([
            detail_line(
                "cache",
                if session.cache.miss_count == 0 {
                    format!("{} hit · no misses", hit_label(session.cache_hit_ratio()))
                } else {
                    format!(
                        "{} hit · {} {} · {} tokens reprocessed",
                        hit_label(session.cache_hit_ratio()),
                        session.cache.miss_count,
                        if session.cache.miss_count == 1 {
                            "miss"
                        } else {
                            "misses"
                        },
                        compact_tokens(session.cache.missed_tokens)
                    )
                },
                if session.cache.miss_count == 0 {
                    hit_color(session.cache_hit_ratio())
                } else {
                    RED
                },
            ),
            detail_line(
                "context",
                format!(
                    "{} now · {} peak{}",
                    compact_tokens(session.cache.context_tokens),
                    compact_tokens(session.cache.peak_context_tokens),
                    context_cache_expiry_label(&session.cache, SystemTime::now())
                ),
                DIM_WHITE,
            ),
            detail_line(
                "last miss",
                session
                    .cache
                    .last_miss
                    .map(|(at, miss)| {
                        format!("{} · {}", format_system_time(at), cache_miss_summary(&miss))
                    })
                    .unwrap_or_else(|| "-".to_string()),
                if session.cache.last_miss.is_some() {
                    RED
                } else {
                    DIM
                },
            ),
            detail_line("rate", session.rate().label(), TEAL),
            detail_line(
                "last status",
                session.last_status.as_str(),
                status_color(&session.last_status),
            ),
        ]);
        lines
    } else {
        vec![Line::from(Span::styled(
            "No session selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Session detail", true)),
        area,
    );
}

fn render_request_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorState,
    selected: usize,
) {
    let lines = if let Some(request) = state.recent.get(selected) {
        let mut lines = vec![
            detail_line("request", request.request_id.clone(), WHITE),
            detail_line(
                "session",
                match request.conversation.as_deref() {
                    Some(conversation) => format!(
                        "{} · {}",
                        display_session_id(request.session_id.as_deref()),
                        conversation
                    ),
                    None => display_session_id(request.session_id.as_deref()).to_string(),
                },
                TEAL,
            ),
            detail_line(
                "session seq",
                request
                    .session_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                DIM_WHITE,
            ),
            detail_line("endpoint", request.endpoint.label(), DIM_WHITE),
            detail_line("started", format_system_time(request.started_at), DIM_WHITE),
            detail_line(
                "finished",
                format_system_time(request.finished_at),
                DIM_WHITE,
            ),
            detail_line(
                "status",
                request.status.label(),
                status_color(request.status.label()),
            ),
            detail_line(
                "http status",
                request
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                http_status_color(request.http_status),
            ),
            detail_line("provider", request.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line(
                "model",
                request_model_summary(
                    request.provider.as_deref(),
                    request.requested_model.as_deref(),
                    request.effective_model.as_deref(),
                ),
                DIM_WHITE,
            ),
            detail_line("effort", request.effort.as_deref().unwrap_or("-"), YELLOW),
            // The pane is a share of the terminal and does not scroll, so the
            // counts come before the timings: they are what a short pane has
            // to keep.
            detail_line(
                "tokens",
                request_token_summary(
                    request.prompt_tokens(),
                    request.input_tokens,
                    request.output_tokens,
                    &request.cache,
                    request.usage_quality(),
                    request.cache_write_quality(),
                ),
                DIM_WHITE,
            ),
            detail_line(
                "cache",
                format!(
                    "{} · {}",
                    match (request.cache.miss, request.cache.evaluated()) {
                        (Some(miss), _) => format!(
                            "{} hit · miss {}",
                            hit_label(request.cache_hit_ratio()),
                            cache_miss_summary(&miss)
                        ),
                        (None, true) => {
                            format!("{} hit · no miss", hit_label(request.cache_hit_ratio()))
                        }
                        (None, false) => hit_label(request.cache_hit_ratio()),
                    },
                    cache_write_bucket_summary(&request.cache, request.cache_write_quality())
                ),
                if request.cache.miss.is_some() {
                    RED
                } else {
                    hit_color(request.cache_hit_ratio())
                },
            ),
            detail_line("latency", format_duration(request.latency), DIM_WHITE),
            detail_line("rate", request.rate().label(), TEAL),
            detail_line(
                "stream",
                format!(
                    "{} bytes · {} chunks",
                    request.streamed_bytes, request.stream_chunks
                ),
                DIM_WHITE,
            ),
        ];
        if let Some(error) = request.error.as_deref().filter(|error| !error.is_empty()) {
            lines.push(detail_line("detail", error, YELLOW));
        }
        if let Some(path) = &request.traffic_capture_path {
            lines.push(detail_line(
                "capture",
                path.to_string_lossy().into_owned(),
                DIM_WHITE,
            ));
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "No request selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Request detail", true)),
        area,
    );
}

fn detail_line<'a>(label: &'static str, value: impl Into<String>, value_color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<16}"), Style::default().fg(DIM)),
        Span::styled(value.into(), Style::default().fg(value_color)),
    ])
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, _app: &MonitorApp) {
    let spans = vec![
        Span::raw(" "),
        Span::styled("q", Style::default().fg(TEAL)),
        Span::styled(" quit  ", Style::default().fg(DIM)),
        Span::styled("?", Style::default().fg(TEAL)),
        Span::styled(" help  ", Style::default().fg(DIM)),
        Span::styled("b", Style::default().fg(TEAL)),
        Span::styled(" setup  ", Style::default().fg(DIM)),
        Span::styled("arrows/j/k", Style::default().fg(TEAL)),
        Span::styled(" navigate  ", Style::default().fg(DIM)),
        Span::styled("Tab", Style::default().fg(TEAL)),
        Span::styled(" pane  ", Style::default().fg(DIM)),
        Span::styled("Enter", Style::default().fg(TEAL)),
        Span::styled(" open", Style::default().fg(DIM)),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

fn render_shutdown_confirmation(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let width = 44.min(area.width);
    let height = 5.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Shut down proxy?",
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("y", Style::default().fg(TEAL)),
                Span::styled(" confirm   ", Style::default().fg(DIM_WHITE)),
                Span::styled("n/Esc/q", Style::default().fg(TEAL)),
                Span::styled(" cancel", Style::default().fg(DIM_WHITE)),
            ]),
        ])
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(YELLOW))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().bg(BG)),
        popup,
    );
}

fn render_shutdown_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, tick: usize) {
    let width = 40.min(area.width);
    let height = 5.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("{} ", spinner(tick)), Style::default().fg(TEAL)),
                Span::styled(
                    "Shutting down...",
                    Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "Press Ctrl-C to force quit",
                Style::default().fg(DIM_WHITE),
            )),
        ])
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(TEAL))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().bg(BG)),
        popup,
    );
}

fn render_help_overlay(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let width = 48.min(area.width.saturating_sub(4)).max(24);
    let height = 12.min(area.height.saturating_sub(2)).max(8);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" Shortcuts ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines = [
        ("q / Ctrl-C", "quit proxy"),
        ("?", "toggle help"),
        ("b", "toggle setup"),
        ("arrows", "navigate rows and panes"),
        ("j / k", "previous / next row"),
        ("Tab", "switch pane"),
        ("Enter", "open detail"),
        ("Esc", "close overlay / detail"),
    ];
    let content = lines
        .into_iter()
        .map(|(key, label)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{key:<10}"), Style::default().fg(TEAL)),
                Span::styled(label, Style::default().fg(DIM_WHITE)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(content).style(Style::default().bg(BG)),
        inner,
    );
}

fn render_setup_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, setup_text: &str) {
    let width = 84.min(area.width.saturating_sub(4)).max(36);
    let content_height = setup_text.lines().count() as u16;
    let height = (content_height + 4)
        .min(area.height.saturating_sub(2))
        .max(8);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" Setup ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = setup_text
        .lines()
        .map(|line| {
            let style = if line.starts_with("export ") {
                Style::default().fg(WHITE)
            } else {
                Style::default().fg(DIM_WHITE)
            };
            Line::from(vec![Span::raw("  "), Span::styled(line.to_string(), style)])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Esc", Style::default().fg(TEAL)),
        Span::styled(" close  ", Style::default().fg(DIM)),
        Span::styled("b", Style::default().fg(TEAL)),
        Span::styled(" toggle setup", Style::default().fg(DIM)),
    ]));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn mock_setup_text(port: u16, registry: &Registry) -> String {
    format!(
        "Mock mode uses deterministic simulated monitor traffic.\nNo proxy server is listening.\nRun `claude-code-mux serve` to start the proxy.\n\n{}",
        setup_text(port, registry)
    )
}

pub fn setup_text(port: u16, registry: &Registry) -> String {
    let grouped = registry.grouped_models();
    let model_summary = ["codex", "kimi", "cursor"]
        .into_iter()
        .filter_map(|provider| {
            grouped
                .get(provider)
                .map(|models| format!("{provider}: {} models", models.len()))
        })
        .collect::<Vec<_>>()
        .join("  ");
    let mut lines = vec![
        format!("Logs: {}", paths::log_file().display()),
        format!("Config: {}", paths::config_dir().display()),
        format!("Providers: {model_summary}"),
    ];
    lines.push(format!(
        "export ANTHROPIC_BASE_URL=\"http://localhost:{port}\""
    ));
    lines.push("export ANTHROPIC_AUTH_TOKEN=\"anything\"".to_string());
    lines.push("export ANTHROPIC_MODEL=\"gpt-5.6-sol\"".to_string());
    lines.push("export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-5.6-luna\"".to_string());
    lines.push("export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1".to_string());
    lines.join("\n")
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_system_time(time: SystemTime) -> String {
    format_system_time_in_zone(time, TimeZone::system())
}

fn format_system_time_in_zone(time: SystemTime, time_zone: TimeZone) -> String {
    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "-".to_string();
    };
    Zoned::new(timestamp, time_zone)
        .strftime("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::monitor::{EndpointKind, UsageReport, mock_state};

    fn draw(width: u16, height: u16, render: impl FnOnce(&mut ratatui::Frame<'_>)) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(render).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn placeholder_position(buffer: &Buffer, placeholder: &str) -> Option<(u16, u16)> {
        let symbols = placeholder
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        (0..buffer.area.height).find_map(|y| {
            (0..buffer.area.width).find_map(|x| {
                symbols
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| {
                        x + (offset as u16) < buffer.area.width
                            && buffer[(x + offset as u16, y)].symbol() == symbol
                    })
                    .then_some((x, y))
            })
        })
    }

    fn assert_centered(buffer: &Buffer, placeholder: &str, expected_y: u16) {
        let (x, y) = placeholder_position(buffer, placeholder).unwrap();
        let left_space = x.saturating_sub(1);
        let right_space = buffer
            .area
            .width
            .saturating_sub(x + placeholder.chars().count() as u16 + 1);
        assert_eq!(y, expected_y);
        assert!(left_space.abs_diff(right_space) <= 1);
    }

    fn headers<K>(columns: &[ColumnSpec<K>]) -> Vec<&'static str> {
        columns.iter().map(|column| column.header).collect()
    }

    fn fixed_budget<K>(columns: &[ColumnSpec<K>]) -> u16 {
        let widths = columns
            .iter()
            .map(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => width,
                layout::ColumnWidth::Flex(_) => 0,
            })
            .sum::<u16>();
        widths.saturating_add(columns.len().saturating_sub(1) as u16)
    }

    fn fixed_width<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<u16> {
        columns
            .iter()
            .find(|column| column.key == key)
            .and_then(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => Some(width),
                layout::ColumnWidth::Flex(_) => None,
            })
    }

    fn flex_count<K>(columns: &[ColumnSpec<K>]) -> usize {
        columns
            .iter()
            .filter(|column| matches!(column.width, layout::ColumnWidth::Flex(_)))
            .count()
    }

    fn alignment<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<Alignment> {
        columns
            .iter()
            .find(|column| column.key == key)
            .map(|column| column.alignment)
    }

    #[test]
    fn format_system_time_applies_non_utc_time_zone() {
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(20 * 60 * 60);
        let time_zone = TimeZone::fixed(jiff::tz::offset(5));

        assert_eq!(format_system_time_in_zone(timestamp, time_zone), "01:00:00");
    }

    #[test]
    fn request_tables_share_time_status_provider_model_rhythm() {
        assert_eq!(
            headers(&active_columns(LayoutTier::Medium))[..4],
            ["Started", "Status", "Provider", "Model"]
        );
        assert_eq!(
            headers(&recent_columns(LayoutTier::Medium))[..4],
            ["Finished", "Code", "Provider", "Model"]
        );
        assert_eq!(
            headers(&event_columns(LayoutTier::Medium))[..4],
            ["Time", "Code", "Provider", "Model"]
        );
    }

    #[test]
    fn wide_tables_use_shared_model_and_provider_widths() {
        let sessions = session_columns(LayoutTier::Wide, true);
        let active = active_columns(LayoutTier::Wide);
        let recent = recent_columns(LayoutTier::Wide);
        let events = event_columns(LayoutTier::Wide);

        assert_eq!(
            fixed_width(&sessions, SessionColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&events, EventColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
    }

    #[test]
    fn responsive_schemas_fit_their_minimum_terminal_widths() {
        assert!(fixed_budget(&session_columns(LayoutTier::Emergency, false)) <= 75);
        assert!(fixed_budget(&session_columns(LayoutTier::Narrow, false)) <= 76);
        assert!(fixed_budget(&session_columns(LayoutTier::Medium, false)) <= 88);
        assert!(fixed_budget(&session_columns(LayoutTier::Expanded, false)) <= 118);
        // The short sparkline header must still fit at the first wide width.
        assert!(fixed_budget(&session_columns(LayoutTier::Wide, false)) <= 152 - 10);
        // The full sparkline header must fit where the full sparkline starts.
        assert!(
            fixed_budget(&session_columns(LayoutTier::Wide, true))
                <= SESSION_SPARKLINE_MIN_WIDTH - 2 - "Tokens/10s · 4k".chars().count() as u16
        );

        assert!(fixed_budget(&active_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&active_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&active_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&active_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&active_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&recent_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&recent_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&recent_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&recent_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&recent_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&event_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&event_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&event_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&event_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&event_columns(LayoutTier::Wide)) <= 152);
    }

    #[test]
    fn active_table_renders_expected_headers_at_tier_boundaries() {
        let state = mock_state();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_active(frame, frame.area(), &state.active, 0)
            });
            buffer_text(&buffer)
        };

        let emergency = render_at(77);
        assert!(emergency.contains("Started"), "{emergency}");
        assert!(emergency.contains("Target"), "{emergency}");
        assert!(!emergency.contains("Rate"), "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Provider"), "{narrow}");
        assert!(narrow.contains("Model"), "{narrow}");
        assert!(narrow.contains("Effort"), "{narrow}");
        assert!(!narrow.contains("Project"), "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Provider"), "{medium}");
        assert!(medium.contains("Model"), "{medium}");
        assert!(medium.contains("Endpoint"), "{medium}");
        assert!(!medium.contains("Project"), "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Project"), "{expanded}");
        assert!(expanded.contains("Session"), "{expanded}");
        assert!(expanded.contains("Endpoint"), "{expanded}");
        assert!(!expanded.contains("In"), "{expanded}");

        let wide = render_at(154);
        assert!(wide.contains("Project"), "{wide}");
        assert!(wide.contains("Session"), "{wide}");
        assert!(wide.contains("In"), "{wide}");
        assert!(wide.contains("Out"), "{wide}");
    }

    #[test]
    fn each_schema_has_one_meaningful_flexible_column() {
        for tier in [
            LayoutTier::Emergency,
            LayoutTier::Narrow,
            LayoutTier::Medium,
            LayoutTier::Expanded,
            LayoutTier::Wide,
        ] {
            let sessions = session_columns(tier, tier == LayoutTier::Wide);
            let active = active_columns(tier);
            let recent = recent_columns(tier);
            let events = event_columns(tier);

            assert_eq!(flex_count(&sessions), 1);
            assert_eq!(flex_count(&active), 1);
            assert_eq!(flex_count(&recent), 1);
            assert_eq!(flex_count(&events), 1);
            assert_eq!(
                sessions
                    .iter()
                    .filter(|column| column.header.is_empty())
                    .count(),
                1
            );
            assert!(active.iter().all(|column| !column.header.is_empty()));
            assert!(recent.iter().all(|column| !column.header.is_empty()));
            assert!(events.iter().all(|column| !column.header.is_empty()));
        }
    }

    #[test]
    fn metric_columns_are_right_aligned() {
        let sessions = session_columns(LayoutTier::Wide, true);
        for key in [
            SessionColumn::Active,
            SessionColumn::Requests,
            SessionColumn::Failures,
            SessionColumn::Input,
            SessionColumn::Output,
            SessionColumn::Rate,
        ] {
            assert_eq!(alignment(&sessions, key), Some(Alignment::Right));
        }

        let active = active_columns(LayoutTier::Wide);
        for key in [
            ActiveColumn::Input,
            ActiveColumn::Output,
            ActiveColumn::Rate,
            ActiveColumn::Elapsed,
        ] {
            assert_eq!(alignment(&active, key), Some(Alignment::Right));
        }

        let recent = recent_columns(LayoutTier::Wide);
        for key in [
            RecentColumn::Code,
            RecentColumn::Latency,
            RecentColumn::Rate,
            RecentColumn::Input,
            RecentColumn::Output,
        ] {
            assert_eq!(alignment(&recent, key), Some(Alignment::Right));
        }
    }

    #[test]
    fn narrow_schemas_use_available_space_for_context() {
        let sessions = session_columns(LayoutTier::Narrow, false);
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Project)
        );
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Target)
        );

        let active = active_columns(LayoutTier::Narrow);
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Provider)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Model)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Effort)
        );

        let recent = recent_columns(LayoutTier::Narrow);
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Provider)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Input)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Output)
        );

        let events = event_columns(LayoutTier::Emergency);
        assert_eq!(headers(&events), ["Time", "Code", "Message"]);
    }

    #[test]
    fn display_session_id_shortens_uuids() {
        assert_eq!(
            display_session_id(Some("57c7c914-ada4-4f40-9672-985f950fbb66")),
            "57c7c914"
        );
    }

    #[test]
    fn display_conversation_label_shortens_agent_ids_and_keeps_side_calls() {
        assert_eq!(display_conversation_label("main"), "main");
        assert_eq!(display_conversation_label("main/side"), "main/side");
        assert_eq!(display_conversation_label("a0c17aa4e68872d70"), "a0c17aa4");
        assert_eq!(
            display_conversation_label("a0c17aa4e68872d70/side"),
            "a0c17aa4/side"
        );
        assert_eq!(
            display_conversation_label("57c7c914-ada4-4f40-9672-985f950fbb66"),
            "57c7c914"
        );
    }

    #[test]
    fn display_session_id_handles_atypical_ids() {
        assert_eq!(display_session_id(Some("custom-session")), "custom-session");
        assert_eq!(display_session_id(Some("")), "no-session");
        assert_eq!(display_session_id(None), "no-session");
    }

    #[test]
    fn ellipsize_marks_truncated_values() {
        assert_eq!(ellipsize("claude-sonnet-4-6", 16), "claude-sonnet-4…");
        assert_eq!(ellipsize("gpt-5.6-sol", 16), "gpt-5.6-sol");
        assert_eq!(ellipsize("anything", 0), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_wall_clock_buckets() {
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(78), 2_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(85), 3_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(100), 4_000),
        ];
        let bucket_start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let bucket_end = SystemTime::UNIX_EPOCH + Duration::from_secs(109);
        let next_bucket = SystemTime::UNIX_EPOCH + Duration::from_secs(110);

        assert_eq!(token_sparkline(&[], 4, bucket_start), "    ");
        assert_eq!(token_sparkline(&samples, 4, bucket_start), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, bucket_end), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, next_bucket), "▆ █ ");
        assert_eq!(token_sparkline(&samples, 0, bucket_start), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_shared_scale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let current = (now, 2_000);
        let offscreen_peak = (SystemTime::UNIX_EPOCH, 4_000);

        assert_eq!(token_sparkline(&[current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[offscreen_peak, current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[(now, 10_000)], 1, now), "█");
    }

    #[test]
    fn token_sparkline_dims_the_current_bucket() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(95), 2_000),
            (now, 4_000),
        ];

        let line = token_sparkline_line(&samples, 2, now);

        assert_eq!(line.spans[0].content, "▄");
        assert_eq!(line.spans[0].style.fg, Some(BLUE));
        assert_eq!(line.spans[1].content, "█");
        assert_eq!(line.spans[1].style.fg, Some(DIM));
    }

    #[test]
    fn session_sparkline_appears_at_medium_width_and_expands() {
        let monitor = MonitorHandle::new(10);
        for (index, tokens) in [1_000, 2_000, 4_000].into_iter().enumerate() {
            let request_id = format!("request-{index}");
            monitor.request_started(
                &request_id,
                Some("sess-1".to_string()),
                Some(index as u64 + 1),
                EndpointKind::Messages,
            );
            monitor.provider_selected(&request_id, "codex", "gpt-5.6-sol", None);
            monitor.request_completed(&request_id, 200, Some(100), Some(tokens));
        }
        let state = monitor.snapshot();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_sessions(
                    frame,
                    frame.area(),
                    &state.sessions,
                    SelectionView::at(0),
                    true,
                )
            });
            buffer_text(&buffer)
        };
        let spark_chars = |text: &str| {
            text.chars()
                .filter(|ch| matches!(ch, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'))
                .count()
        };

        let emergency = render_at(77);
        assert!(!emergency.contains("Trend"), "{emergency}");
        assert_eq!(spark_chars(&emergency), 0, "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Trend"), "{narrow}");
        assert!(spark_chars(&narrow) > 0, "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Tok/10s"), "{medium}");
        assert!(spark_chars(&medium) > 0, "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Tokens/10s"), "{expanded}");
        assert!(spark_chars(&expanded) > 0, "{expanded}");

        let wide = render_at(SESSION_SPARKLINE_MIN_WIDTH);
        assert!(wide.contains("Tokens/10s · 4k"), "{wide}");
        assert!(spark_chars(&wide) > 0, "{wide}");
    }

    #[test]
    fn empty_tables_hide_columns_and_center_placeholders() {
        let sessions = draw(40, 9, |frame| {
            render_sessions(frame, frame.area(), &[], SelectionView::at(0), true)
        });
        let sessions_text = buffer_text(&sessions);
        assert_centered(&sessions, "No sessions", 4);
        assert!(!sessions_text.contains("provider"));
        assert!(sessions_text.contains("No sessions"));

        let active = draw(27, 6, |frame| render_active(frame, frame.area(), &[], 0));
        let active_text = buffer_text(&active);
        assert_centered(&active, "No active requests", 2);
        assert!(!active_text.contains("started"));
        assert!(active_text.contains("No active requests"));

        let recent = draw(40, 9, |frame| {
            render_recent(frame, frame.area(), &[], SelectionView::at(0), false)
        });
        let recent_text = buffer_text(&recent);
        assert_centered(&recent, "No recent requests", 4);
        assert!(!recent_text.contains("finished"));
        assert!(recent_text.contains("No recent requests"));

        let events = draw(40, 9, |frame| render_events(frame, frame.area(), &[]));
        let events_text = buffer_text(&events);
        assert_centered(&events, "No events", 4);
        assert!(!events_text.contains("time"));
        assert!(events_text.contains("No events"));
    }

    #[test]
    fn active_status_keeps_full_label_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.upstream_started("request-1");
        let state = monitor.snapshot();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋ upstream"), "{active_text}");
    }

    #[test]
    fn active_compacting_status_is_distinct_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.compaction_started("request-1");
        let state = monitor.snapshot();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋compacting"), "{active_text}");
        assert_eq!(status_color("compacting"), PURPLE);
    }

    #[test]
    fn cache_columns_show_hit_rate_context_and_misses() {
        let state = mock_state();
        let session_index = state
            .sessions
            .iter()
            .position(|session| {
                session.session_id.as_deref() == Some("57c7c914-ada4-4f40-9672-985f950fbb66")
            })
            .unwrap();

        let sessions = buffer_text(&draw(170, 12, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(session_index),
                true,
            )
        }));
        for header in ["Ctx", "Hit", "Miss"] {
            assert!(sessions.contains(header), "{sessions}");
        }
        assert!(sessions.contains("1/118.2k"), "{sessions}");

        let recent = buffer_text(&draw(154, 12, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                true,
            )
        }));
        for header in ["Hit", "Miss"] {
            assert!(recent.contains(header), "{recent}");
        }
        assert!(recent.contains("118.2k"), "{recent}");
        let roomy_recent = buffer_text(&draw(200, 12, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                true,
            )
        }));
        assert!(
            roomy_recent.contains("cache miss · expired · idle 34m00s > ttl 30m00s"),
            "{roomy_recent}"
        );

        let request = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-complete-codex")
            .unwrap();
        let detail = buffer_text(&draw(140, 24, |frame| {
            render_request_detail(frame, frame.area(), &state, request)
        }));
        // The Codex backend closed the three counts it reports and never
        // mentioned a cache write, which is not a write of zero.
        assert!(
            detail.contains(
                "125.6k prompt · 121.9k in · 3.7k read · n/a write · 832 out \
                 · ~ provisional · n/a not reported"
            ),
            "{detail}"
        );
        assert!(
            detail.contains("requested claude-sonnet-4-6 · executed gpt-5.6-terra"),
            "{detail}"
        );
        assert!(
            detail.contains("3% hit · miss 118.2k of 121.9k"),
            "{detail}"
        );
        assert!(
            detail.contains("write parts reported separately: 5m n/a, 1h n/a"),
            "{detail}"
        );

        // The Anthropic subagent measured every count and named both lifetimes
        // it wrote under, so its detail carries no mark and no legend.
        let exact = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-complete-subagent")
            .unwrap();
        let exact_detail = buffer_text(&draw(140, 24, |frame| {
            render_request_detail(frame, frame.area(), &state, exact)
        }));
        assert!(
            exact_detail.contains("43.7k prompt · 4.1k in · 38.4k read · 1.2k write · 310 out"),
            "{exact_detail}"
        );
        assert!(
            exact_detail.contains("write parts reported separately: 5m 900, 1h 300"),
            "{exact_detail}"
        );
        assert!(!exact_detail.contains("provisional"), "{exact_detail}");

        // The other Anthropic subagent named one lifetime and said nothing
        // about the other, so the bucket it left out stays unknown instead of
        // becoming the rest of the write.
        let mixed = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-complete-orphan")
            .unwrap();
        let mixed_detail = buffer_text(&draw(140, 24, |frame| {
            render_request_detail(frame, frame.area(), &state, mixed)
        }));
        assert!(
            mixed_detail.contains("write parts reported separately: 5m 640, 1h n/a"),
            "{mixed_detail}"
        );

        // The Codex subagent's stream ended before a closing report, so every
        // count it holds is still the estimate the stream opened with.
        let opening = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-complete-nested")
            .unwrap();
        let opening_detail = buffer_text(&draw(140, 24, |frame| {
            render_request_detail(frame, frame.area(), &state, opening)
        }));
        assert!(
            opening_detail.contains("~18.9k prompt · ~6.8k in · ~12.1k read · n/a write · ~96 out"),
            "{opening_detail}"
        );

        let session_detail = buffer_text(&draw(140, 20, |frame| {
            render_session_detail(frame, frame.area(), &state, session_index)
        }));
        assert!(
            session_detail.contains("1 miss · 118.2k tokens reprocessed"),
            "{session_detail}"
        );
        assert!(session_detail.contains("context"), "{session_detail}");
    }

    #[test]
    fn populated_tables_render_rows_without_placeholders() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.project_resolved("request-1", "example-project");
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        let active_state = monitor.snapshot();

        let sessions = draw(170, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &active_state.sessions,
                SelectionView::at(0),
                true,
            )
        });
        let sessions_text = buffer_text(&sessions);
        assert!(sessions_text.contains("Provider"));
        assert!(sessions_text.contains("Project"));
        assert!(sessions_text.contains("example-project"));
        assert!(sessions_text.contains("sess-1"));
        assert!(!sessions_text.contains("No sessions"));

        let active = draw(120, 8, |frame| {
            render_active(frame, frame.area(), &active_state.active, 0)
        });
        let active_text = buffer_text(&active);
        assert!(active_text.contains("Started"));
        assert!(active_text.contains("gpt-5.6-sol"));
        assert!(!active_text.contains("No active requests"));

        monitor.request_completed("request-1", 200, Some(100), Some(25));
        let completed_state = monitor.snapshot();
        let recent = draw(140, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &completed_state.recent,
                SelectionView::at(0),
                false,
            )
        });
        let recent_text = buffer_text(&recent);
        assert!(recent_text.contains("Finished"));
        assert!(recent_text.contains("200"));
        assert!(!recent_text.contains("No recent requests"));

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &completed_state.recent)
        });
        assert!(buffer_text(&events).contains("No events"));
    }

    #[test]
    fn sessions_table_nests_conversations_and_selects_the_row_it_marks() {
        let monitor = MonitorHandle::new(10);
        for (request_id, conversation, parent, provider, model) in [
            ("request-main", "main", None, "codex", "gpt-5.6-sol"),
            (
                "request-agent",
                "agent-7",
                None,
                "anthropic",
                "claude-sonnet-5",
            ),
            (
                "request-nested",
                "agent-9",
                Some("agent-7"),
                "codex",
                "gpt-5.6-luna",
            ),
            (
                "request-label",
                "agent-9/side",
                Some("agent-7"),
                "codex",
                "gpt-5.6-luna",
            ),
        ] {
            monitor.request_started(
                request_id,
                Some("sess-1".to_string()),
                None,
                EndpointKind::Messages,
            );
            monitor.project_resolved(request_id, "example-project");
            monitor.conversation_resolved(request_id, conversation, parent.map(str::to_string));
            monitor.provider_selected(request_id, provider, model, None);
            monitor.request_completed(request_id, 200, Some(1_000), Some(50));
        }
        let state = monitor.snapshot();

        // One session row, then its conversations depth first.
        let rows = session_rows(&state.sessions);
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], SessionRow::Session(_)));
        let labels = rows[1..]
            .iter()
            .map(|row| row.id_label(40))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "├─main thread",
                "└─agent-7",
                " └─agent-9",
                "  └─agent-9/side"
            ]
        );
        // Every conversation row answers for the session it hangs under.
        for row in 1..rows.len() {
            assert_eq!(selected_session_index(&state.sessions, row), Some(0));
        }

        let sessions = buffer_text(&draw(170, 10, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(4),
                true,
            )
        }));

        assert!(sessions.contains("├─main"), "{sessions}");
        assert!(sessions.contains(" └─agent-9"), "{sessions}");
        // The marker sits on the row the selection index points at.
        assert!(sessions.contains(">   └─agent-9/side"), "{sessions}");
        assert!(sessions.contains("claude-sonnet-5"), "{sessions}");
        assert_eq!(sessions.matches("example-project").count(), 1, "{sessions}");
        assert_eq!(sessions.matches("sess-1").count(), 1, "{sessions}");
    }

    #[test]
    fn session_rows_read_as_the_aggregate_they_are_and_name_the_main_thread() {
        let monitor = MonitorHandle::new(10);
        for (request_id, conversation) in [("request-main", "main"), ("request-agent", "agent-7")] {
            monitor.request_started(
                request_id,
                Some("sess-1".to_string()),
                None,
                EndpointKind::Messages,
            );
            monitor.conversation_resolved(request_id, conversation, None);
            monitor.provider_selected(request_id, "codex", "gpt-5.6-sol", None);
            monitor.model_resolved(request_id, "gpt-5.6-sol");
            monitor.request_completed(request_id, 200, Some(1_000), Some(50));
        }
        let state = monitor.snapshot();
        let rows = session_rows(&state.sessions);

        // The session row is every request counted once, the same requests its
        // conversation rows project another way. The mark says so in the label,
        // not by the indent the conversations happen to carry.
        assert_eq!(rows[0].id_label(16), "Σ sess-1");
        let labels = rows[1..]
            .iter()
            .map(|row| row.id_label(16))
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["├─main thread", "└─agent-7"]);
        // A column too narrow for the spelled name keeps the short one whole
        // rather than a truncation mark.
        assert_eq!(rows[1].id_label(8), "├─main");

        let sessions = buffer_text(&draw(170, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        assert!(sessions.contains("Σ sess-1"), "{sessions}");
        assert!(sessions.contains("├─main thread"), "{sessions}");
    }

    /// A request that names a session and nothing else, which is all a
    /// Sessions row needs.
    fn record_session_request(
        monitor: &MonitorHandle,
        request_id: &str,
        session: &str,
        project: &str,
    ) {
        monitor.request_started(
            request_id,
            Some(session.to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.project_resolved(request_id, project);
        monitor.provider_selected(request_id, "codex", "gpt-5.6-sol", None);
        monitor.request_completed(request_id, 200, Some(1_000), Some(50));
    }

    /// A request of one conversation of a session, with the parent Claude Code
    /// named for it.
    fn record_conversation_request(
        monitor: &MonitorHandle,
        request_id: &str,
        session: &str,
        conversation: &str,
        parent: Option<&str>,
    ) {
        monitor.request_started(
            request_id,
            Some(session.to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved(request_id, conversation, parent.map(str::to_string));
        monitor.provider_selected(request_id, "codex", "gpt-5.6-sol", None);
        monitor.request_completed(request_id, 200, Some(1_000), Some(50));
    }

    fn monitor_app(focus: FocusPane) -> MonitorApp {
        MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus,
            selected: Selection::default(),
            recent_selected: Selection::default(),
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        }
    }

    /// One turn of the monitor loop: point both panes at the rows they picked
    /// in a fresh snapshot, then draw the screen that comes out.
    fn draw_monitor(app: &mut MonitorApp, state: &MonitorState, width: u16, height: u16) -> String {
        app.sync_selection(&session_rows(&state.sessions), &state.recent);
        buffer_text(&draw(width, height, |frame| render(frame, app, state)))
    }

    fn move_down_in(app: &mut MonitorApp, state: &MonitorState) {
        app.move_down(&session_rows(&state.sessions), &state.recent, false);
    }

    #[test]
    fn conversation_rows_mark_a_parent_the_session_never_saw() {
        let monitor = MonitorHandle::new(10);
        for (request_id, conversation, parent) in [
            ("request-main", "main", None),
            ("request-child", "agent-7", Some("main")),
            ("request-side", "agent-7/side", None),
            ("request-orphan", "agent-8", Some("agent-elsewhere")),
            ("request-cycle-one", "agent-a", Some("agent-b")),
            ("request-cycle-two", "agent-b", Some("agent-a")),
        ] {
            record_conversation_request(&monitor, request_id, "sess-1", conversation, parent);
        }
        let state = monitor.snapshot();
        let labels = session_rows(&state.sessions)[1..]
            .iter()
            .map(|row| row.id_label(24))
            .collect::<Vec<_>>();

        // The walk placed the child and the side lane, which the suffix keeps
        // apart from a conversation of its own. It could not place the other
        // three — one named a conversation this session never saw, two named
        // each other — so they hang under the session row like a thread that
        // named no parent at all, and the mark is what tells the two apart.
        assert_eq!(
            labels,
            vec![
                "├─main thread",
                " └─agent-7",
                "  └─agent-7/side",
                "├─^agent-8",
                "├─^agent-a",
                "└─^agent-b",
            ]
        );
    }

    #[test]
    fn the_unknown_parent_mark_survives_the_narrowest_id_column() {
        let monitor = MonitorHandle::new(10);
        record_conversation_request(&monitor, "request-main", "sess-1", "main", None);
        record_conversation_request(
            &monitor,
            "request-orphan",
            "sess-1",
            "agent-8",
            Some("agent-elsewhere"),
        );
        let state = monitor.snapshot();

        // The narrowest tier cuts the id, never the glyphs in front of it:
        // where a row hangs is what the column is for.
        let sessions = buffer_text(&draw(60, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(0),
                false,
            )
        }));
        assert_eq!(LayoutTier::for_outer_width(60), LayoutTier::Emergency);
        assert!(sessions.contains("└─^agent-8"), "{sessions}");
    }

    #[test]
    fn enter_opens_the_detail_of_the_row_a_fresh_pane_marks() {
        let monitor = MonitorHandle::new(10);
        record_session_request(&monitor, "request-1", "sess-1", "project-1");
        let state = monitor.snapshot();
        let mut app = monitor_app(FocusPane::Sessions);
        app.sync_selection(&session_rows(&state.sessions), &state.recent);

        // What Enter asks before it opens the pane. A monitor nobody has moved
        // in marks its first row, and that is the row the detail is of.
        assert!(app.selected.row().is_some());
        app.detail = Some(DetailView::Session);
        let detail = buffer_text(&draw(170, 24, |frame| render(frame, &mut app, &state)));

        assert!(detail.contains("Session detail"), "{detail}");
        assert!(!detail.contains("No session selected"), "{detail}");
    }

    #[test]
    fn an_emptied_pane_still_says_its_selection_was_reset() {
        let monitor = MonitorHandle::new(10);
        record_session_request(&monitor, "request-1", "sess-1", "project-1");
        let state = monitor.snapshot();
        let mut app = monitor_app(FocusPane::Sessions);
        draw_monitor(&mut app, &state, 170, 24);
        move_down_in(&mut app, &state);

        // The row that was picked went with the last session, so there is
        // nothing left to mark and nothing left to fall back to. The empty
        // pane still has to account for the marker that was there.
        let empty = MonitorHandle::new(10).snapshot();
        let text = draw_monitor(&mut app, &empty, 170, 24);

        assert!(text.contains("No sessions"), "{text}");
        assert!(text.contains("Sessions (selection reset)"), "{text}");
    }

    #[test]
    fn the_sessions_pane_keeps_its_row_when_a_session_appears_above_it() {
        let monitor = MonitorHandle::new(10);
        record_session_request(&monitor, "request-b", "sess-b", "project-b");
        let first = monitor.snapshot();
        let mut app = monitor_app(FocusPane::Sessions);
        draw_monitor(&mut app, &first, 170, 24);
        // The row is picked, which is what there is to keep: a pane nobody has
        // touched holds no row of its own and follows the top of the list.
        move_down_in(&mut app, &first);
        let before = draw_monitor(&mut app, &first, 170, 24);
        assert!(before.contains("> Σ sess-b"), "{before}");

        // Sessions are ordered by id, so a session seen later can arrive above
        // the selected row. The row that was picked keeps the selection.
        record_session_request(&monitor, "request-a", "sess-a", "project-a");
        let after = monitor.snapshot();
        let text = draw_monitor(&mut app, &after, 170, 24);

        assert!(text.contains("Σ sess-a"), "{text}");
        assert!(text.contains("> Σ sess-b"), "{text}");
        assert!(!text.contains("> Σ sess-a"), "{text}");
    }

    #[test]
    fn the_sessions_pane_says_so_when_the_row_it_marked_is_gone() {
        let monitor = MonitorHandle::new(10);
        record_conversation_request(&monitor, "request-main", "sess-1", "main", None);
        record_conversation_request(&monitor, "request-agent", "sess-1", "agent-7", None);
        let full = monitor.snapshot();
        let mut app = monitor_app(FocusPane::Sessions);
        draw_monitor(&mut app, &full, 170, 24);
        move_down_in(&mut app, &full);
        move_down_in(&mut app, &full);
        let selected = draw_monitor(&mut app, &full, 170, 24);
        assert!(selected.contains("> └─agent-7"), "{selected}");

        // The conversation is gone. The selection goes to the session it hung
        // under rather than to whichever row took its place, and the pane says
        // it is no longer the row that was picked.
        let without_conversation = MonitorHandle::new(10);
        record_conversation_request(
            &without_conversation,
            "request-main",
            "sess-1",
            "main",
            None,
        );
        let smaller = without_conversation.snapshot();
        let fallback = draw_monitor(&mut app, &smaller, 170, 24);
        assert!(fallback.contains("> Σ sess-1"), "{fallback}");
        assert!(
            fallback.contains("Sessions (selection reset)"),
            "{fallback}"
        );

        // With the session gone too there is no sensible row left, so the pane
        // marks none instead of jumping to a session nobody selected.
        let elsewhere = MonitorHandle::new(10);
        record_session_request(&elsewhere, "request-other", "sess-9", "project-9");
        let other = elsewhere.snapshot();
        let empty = draw_monitor(&mut app, &other, 170, 24);
        assert!(empty.contains("Sessions (selection reset)"), "{empty}");
        assert!(!empty.contains("> Σ"), "{empty}");

        // The next move picks a row again and the notice goes with it.
        move_down_in(&mut app, &other);
        let moved = draw_monitor(&mut app, &other, 170, 24);
        assert!(moved.contains("> Σ sess-9"), "{moved}");
        assert!(!moved.contains("Sessions (selection reset)"), "{moved}");
    }

    #[test]
    fn the_recent_pane_keeps_its_request_when_a_newer_one_finishes() {
        let monitor = MonitorHandle::new(10);
        record_session_request(&monitor, "request-first", "sess-1", "project-1");
        let first = monitor.snapshot();
        let mut app = monitor_app(FocusPane::Recent);
        draw_monitor(&mut app, &first, 170, 24);
        move_down_in(&mut app, &first);
        app.detail = Some(DetailView::Request);
        let before = draw_monitor(&mut app, &first, 170, 24);
        assert!(before.contains("request-first"), "{before}");

        // The recent list grows from the top, so the selected request moves
        // down a row. The detail pane must stay on the request it was opened
        // for rather than follow the row number.
        record_session_request(&monitor, "request-second", "sess-1", "project-1");
        let after = monitor.snapshot();
        let text = draw_monitor(&mut app, &after, 170, 24);

        assert_eq!(app.recent_selected.row(), Some(1));
        assert!(text.contains("request-first"), "{text}");
    }

    #[test]
    fn a_session_row_counts_its_models_instead_of_naming_the_last_one() {
        let mixed = MonitorHandle::new(10);
        for (request_id, provider, model) in [
            ("request-codex", "codex", "gpt-5.6-sol"),
            ("request-anthropic", "anthropic", "claude-sonnet-5"),
        ] {
            mixed.request_started(
                request_id,
                Some("sess-mixed".to_string()),
                None,
                EndpointKind::Messages,
            );
            mixed.provider_selected(request_id, provider, model, None);
            mixed.model_resolved(request_id, model);
            mixed.request_completed(request_id, 200, Some(1_000), Some(50));
        }
        let mixed_state = mixed.snapshot();
        let several = buffer_text(&draw(170, 6, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &mixed_state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        // Two backends and two models ran these figures, so neither the last
        // model nor the last backend is the session's. Both cells say so.
        assert_eq!(several.matches("mixed 2").count(), 2, "{several}");
        assert!(!several.contains("gpt-5.6-sol"), "{several}");
        assert!(!several.contains("claude-sonnet-5"), "{several}");
        assert!(!several.contains("codex"), "{several}");
        assert!(!several.contains("anthropic"), "{several}");

        let single = MonitorHandle::new(10);
        single.request_started(
            "request-one",
            Some("sess-one".to_string()),
            None,
            EndpointKind::Messages,
        );
        single.provider_selected("request-one", "codex", "gpt-5.6-sol", None);
        single.model_resolved("request-one", "gpt-5.6-sol");
        single.request_completed("request-one", 200, Some(1_000), Some(50));
        let single_state = single.snapshot();
        let one = buffer_text(&draw(170, 6, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &single_state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        assert!(one.contains("gpt-5.6-sol"), "{one}");
        assert!(!one.contains("mixed"), "{one}");

        let local = MonitorHandle::new(10);
        local.request_started(
            "request-local",
            Some("sess-local".to_string()),
            None,
            EndpointKind::Messages,
        );
        local.provider_selected("request-local", LOCAL_PROVIDER, "claude-sonnet-5", None);
        local.model_requested("request-local", "claude-sonnet-5");
        local.request_completed("request-local", 200, Some(1_200), Some(6));
        let local_state = local.snapshot();
        let answered = buffer_text(&draw(170, 6, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &local_state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        assert!(answered.contains(LOCAL_ANSWER_LABEL), "{answered}");
        assert!(!answered.contains("claude-sonnet-5"), "{answered}");
    }

    #[test]
    fn a_session_row_names_the_one_backend_that_ran_beside_its_local_answers() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-codex",
            Some("sess-1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-codex", "codex", "gpt-5.6-sol", None);
        monitor.model_resolved("request-codex", "gpt-5.6-sol");
        monitor.request_completed("request-codex", 200, Some(1_000), Some(50));

        monitor.request_started(
            "request-local",
            Some("sess-1".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-local", LOCAL_PROVIDER, "claude-sonnet-5", None);
        monitor.model_requested("request-local", "claude-sonnet-5");
        monitor.request_completed("request-local", 200, Some(1_200), Some(6));
        let state = monitor.snapshot();

        let sessions = buffer_text(&draw(170, 6, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        // Only one backend and one model ran anything; the answers the proxy
        // gave itself went to neither, so they make neither ambiguous.
        assert!(sessions.contains("codex"), "{sessions}");
        assert!(sessions.contains("gpt-5.6-sol"), "{sessions}");
        assert!(!sessions.contains(MIXED_MODELS_LABEL), "{sessions}");
    }

    /// A session of two rollup rows — one backend that reports its counts and
    /// one answer the proxy gave itself — plus a request that named no
    /// conversation.
    fn rollup_session_state() -> MonitorState {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-codex",
            Some("sess-1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("request-codex", "main", None);
        monitor.provider_selected("request-codex", "codex", "gpt-5.6-sol", None);
        monitor.model_requested("request-codex", "claude-sonnet-5");
        monitor.model_resolved("request-codex", "gpt-5.6-sol");
        let mut report = UsageReport::default();
        report.closing.input_tokens = Some(20_000);
        report.closing.cache_read_tokens = Some(5_000);
        report.closing.output_tokens = Some(400);
        monitor.usage_reported("request-codex", report);
        monitor.request_completed("request-codex", 200, None, None);

        monitor.request_started(
            "request-local",
            Some("sess-1".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("request-local", "main", None);
        monitor.provider_selected("request-local", LOCAL_PROVIDER, "claude-sonnet-5", None);
        monitor.model_requested("request-local", "claude-sonnet-5");
        monitor.request_completed("request-local", 200, Some(1_200), Some(6));

        // Same backend and same model as the first, so it joins that rollup;
        // it named no conversation, so it is counted on its own as well.
        monitor.request_started(
            "request-loose",
            Some("sess-1".to_string()),
            Some(3),
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-loose", "codex", "gpt-5.6-sol", None);
        monitor.model_requested("request-loose", "claude-sonnet-5");
        monitor.model_resolved("request-loose", "gpt-5.6-sol");
        monitor.request_failed("request-loose", Some(502), "upstream unavailable");
        monitor.snapshot()
    }

    #[test]
    fn session_detail_lists_what_each_model_cost_with_the_evidence_behind_it() {
        let state = rollup_session_state();
        let detail = buffer_text(&draw(160, 24, |frame| {
            render_session_detail(frame, frame.area(), &state, 0)
        }));

        // The two single-valued fields name the newest request, not the whole
        // session, and say which they are: the row above them counts the
        // models, and the two would read as a contradiction unlabelled.
        assert!(detail.contains("last provider"), "{detail}");
        assert!(detail.contains("last model"), "{detail}");

        assert!(detail.contains("codex/gpt-5.6-sol"), "{detail}");
        assert!(detail.contains("asked: claude-sonnet-5×2"), "{detail}");
        assert!(detail.contains("2 req · 1 err"), "{detail}");
        // One of the two requests behind the count never reported it, so the
        // total is no firmer than a single opening estimate.
        assert!(detail.contains("~20.0k in"), "{detail}");
        // The Codex backend reports no cache creation at all; a zero would
        // claim it reported one.
        assert!(detail.contains("n/a write"), "{detail}");
        assert!(!detail.contains("0 write"), "{detail}");
        // A local answer ran no model and spent nothing a backend metered.
        assert!(detail.contains("local answer"), "{detail}");
        assert!(detail.contains("no tokens counted"), "{detail}");
        // Requests that named no conversation are counted in their own right,
        // never left as the remainder of the rows above.
        assert!(detail.contains("unattributed"), "{detail}");
        assert!(!detail.contains("remainder"), "{detail}");
        // How many requests behind each total reported it, held an estimate,
        // or never reported it.
        assert!(detail.contains("req exact/~/n/a"), "{detail}");
        assert!(detail.contains("write 0/0/2"), "{detail}");
    }

    #[test]
    fn session_detail_keeps_the_first_rollup_and_the_evidence_in_a_short_pane() {
        let state = rollup_session_state();
        // The pane is a share of the terminal and does not scroll, so the
        // rollups and the evidence behind them sit above the fold.
        let detail = buffer_text(&draw(150, 16, |frame| {
            render_session_detail(frame, frame.area(), &state, 0)
        }));

        assert!(detail.contains("codex/gpt-5.6-sol"), "{detail}");
        assert!(detail.contains("req exact/~/n/a"), "{detail}");
    }

    #[test]
    fn session_detail_caps_the_model_rollups_and_says_how_many_it_left_out() {
        let monitor = MonitorHandle::new(10);
        for (index, model) in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-6-astra",
            "claude-sonnet-5",
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = format!("request-{index}");
            monitor.request_started(
                &request_id,
                Some("sess-1".to_string()),
                Some(index as u64 + 1),
                EndpointKind::Messages,
            );
            monitor.provider_selected(&request_id, "codex", model, None);
            monitor.model_resolved(&request_id, model);
            monitor.request_completed(&request_id, 200, Some(1_000), Some(50));
        }
        let state = monitor.snapshot();
        let detail = buffer_text(&draw(160, 24, |frame| {
            render_session_detail(frame, frame.area(), &state, 0)
        }));

        assert!(detail.contains("gpt-5.6-sol"), "{detail}");
        assert!(detail.contains("+2 more"), "{detail}");
        assert!(!detail.contains("gpt-6-astra"), "{detail}");
    }

    #[test]
    fn selected_rows_scroll_into_table_viewports() {
        let state = mock_state();
        let sessions = (0..12)
            .map(|index| {
                let mut session = state.sessions[0].clone();
                session.session_id = Some(format!("row-{index:04}"));
                session
            })
            .collect::<Vec<_>>();
        // The last session's own row, past the conversations of the ones above.
        let last_session_row = session_rows(&sessions[..11]).len();
        let session_buffer = draw(120, 6, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &sessions,
                SelectionView::at(last_session_row),
                true,
            )
        });
        let session_text = buffer_text(&session_buffer);
        assert!(session_text.contains("row-0011"), "{session_text}");
        assert!(!session_text.contains("row-0000"), "{session_text}");

        let recent = (0..12)
            .map(|index| {
                let mut request = state.recent[0].clone();
                request.project = Some(format!("row-{index:04}"));
                request
            })
            .collect::<Vec<_>>();
        let recent_buffer = draw(120, 6, |frame| {
            render_recent(frame, frame.area(), &recent, SelectionView::at(11), true)
        });
        let recent_text = buffer_text(&recent_buffer);
        assert!(recent_text.contains("row-0011"), "{recent_text}");
        assert!(!recent_text.contains("row-0000"), "{recent_text}");
    }

    #[test]
    fn mock_state_renders_representative_panes_at_wide_width() {
        let state = mock_state();
        let mut app = MonitorApp {
            listen_url: "mock://tui-demo".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: Selection::default(),
            recent_selected: Selection::default(),
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };

        let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&buffer);

        assert!(text.contains("mock://tui-demo"), "{text}");
        assert!(text.contains("claude-code-mux"), "{text}");
        assert!(text.contains("streaming"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");

        // The session's own row stands for every request it served, and it ran
        // more than one backend and more than one model, so neither cell names
        // the last of them.
        assert!(text.contains("Σ 57c7c914"), "{text}");
        assert!(text.contains("mixed 2"), "{text}");
        assert!(text.contains("mixed 4"), "{text}");
        // A conversation whose parent the session never saw keeps its mark
        // where the tree glyphs are drawn.
        assert!(text.contains("^agent-16"), "{text}");
        // An alias request reads as the model that answered it, not the one it
        // asked for; a request routed but not yet seen on the wire carries the
        // mark that says so.
        assert!(text.contains("gpt-5.6-terra"), "{text}");
        assert!(text.contains("?grok-composer-2.5-fast"), "{text}");
        // The progress label the proxy answered itself ran no model at all,
        // and the Sessions row of that lane says it the way its request row
        // does rather than pairing a backend with an id it never had.
        assert!(text.contains("local answer"), "{text}");
        let lane = text
            .lines()
            .find(|line| line.contains("agent-3/side"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(lane.contains(LOCAL_ANSWER_LABEL), "{lane}");
        assert!(!text.contains("local/-"), "{text}");
        // Counts still open, and counts no backend reported, each say so. The
        // second is read off the row it belongs to: the request reported its
        // input and never its output, so the mark is a statement about one
        // count rather than about a row with nothing in it, and a cell of some
        // other row saying it would not be that.
        assert!(text.contains("~12.5k"), "{text}");
        let unreported = text
            .lines()
            .find(|line| line.contains("kimi-for-coding") && line.contains("messages"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(unreported.contains("~8.9k"), "{unreported}");
        assert!(unreported.contains("n/a"), "{unreported}");
    }

    /// The Sessions row of a lane the proxy answered itself, in both the cell
    /// that names a model and the one that pairs it with a backend. Routing
    /// records the model the request was on its way to, so the row has one to
    /// refuse to name: nothing ran it.
    #[test]
    fn local_conversation_row_names_no_model_and_no_backend_to_pair_it_with() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-label",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.conversation_resolved("request-label", "agent-9/side", None);
        monitor.model_requested("request-label", "claude-sonnet-5");
        monitor.provider_selected("request-label", LOCAL_PROVIDER, "claude-sonnet-5", None);
        monitor.request_completed("request-label", 200, Some(0), Some(12));
        let state = monitor.snapshot();

        let sessions = buffer_text(&draw(170, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        let lane = sessions
            .lines()
            .find(|line| line.contains("agent-9/side"))
            .unwrap_or_else(|| panic!("{sessions}"));
        assert!(lane.contains(LOCAL_ANSWER_LABEL), "{lane}");
        assert!(!lane.contains("claude-sonnet-5"), "{lane}");

        // The narrowest tiers carry one cell for both, and a backend paired
        // with a model that never ran is what it must not become.
        let paired = buffer_text(&draw(84, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &state.sessions,
                SelectionView::at(0),
                true,
            )
        }));
        assert!(paired.contains(LOCAL_ANSWER_LABEL), "{paired}");
        assert!(!paired.contains("local/"), "{paired}");
    }

    #[test]
    fn mock_request_detail_exposes_error_and_capture_fields() {
        let state = mock_state();
        let failed = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-failed-kimi")
            .unwrap();

        let detail = draw(140, 22, |frame| {
            render_request_detail(frame, frame.area(), &state, failed)
        });
        let text = buffer_text(&detail);

        assert!(text.contains("req-failed-kimi"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
        assert!(text.contains("req-failed-kimi.json"), "{text}");
    }

    #[test]
    fn recent_table_uses_error_indicator_at_medium_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let recent = draw(110, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                true,
            )
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("!"), "{recent_text}");
        assert!(!recent_text.contains("Details"), "{recent_text}");
        assert!(
            !recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn recent_table_keeps_detail_text_at_wide_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let recent = draw(180, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                false,
            )
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("Details"), "{recent_text}");
        assert!(
            recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn request_detail_renders_full_error_text() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(7),
            EndpointKind::Messages,
        );
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let detail = draw(120, 20, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        });
        let detail_text = buffer_text(&detail);

        assert!(detail_text.contains("Request detail"), "{detail_text}");
        assert!(detail_text.contains("request-1"), "{detail_text}");
        assert!(detail_text.contains("sess-1"), "{detail_text}");
        assert!(
            detail_text.contains("upstream unavailable"),
            "{detail_text}"
        );
    }

    #[test]
    fn request_rows_name_the_model_that_ran_and_mark_one_only_asked_for() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-executed",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-executed", "codex", "gpt-5.6-sol", None);
        monitor.model_requested("request-executed", "claude-sonnet-5");
        monitor.model_resolved("request-executed", "gpt-5.6-terra");
        let active_state = monitor.snapshot();

        let active = buffer_text(&draw(154, 6, |frame| {
            render_active(frame, frame.area(), &active_state.active, 0)
        }));
        assert!(active.contains("gpt-5.6-terra"), "{active}");
        assert!(!active.contains('?'), "{active}");

        monitor.request_completed("request-executed", 200, Some(10), Some(2));
        // No provider was seen putting this one on the wire.
        monitor.request_started(
            "request-asked",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-asked", "codex", "gpt-5.6-sol", None);
        monitor.model_requested("request-asked", "claude-sonnet-5");
        monitor.request_completed("request-asked", 200, Some(10), Some(2));
        let state = monitor.snapshot();

        let recent = buffer_text(&draw(154, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                false,
            )
        }));
        assert!(recent.contains("gpt-5.6-terra"), "{recent}");
        assert!(recent.contains("?claude-sonnet-5"), "{recent}");

        // The mark is spelled out where there is room for a sentence.
        let asked = state
            .recent
            .iter()
            .position(|request| request.request_id == "request-asked")
            .unwrap();
        let detail = buffer_text(&draw(150, 26, |frame| {
            render_request_detail(frame, frame.area(), &state, asked)
        }));
        assert!(
            detail.contains("requested claude-sonnet-5 · executed not observed (rows mark it ?)"),
            "{detail}"
        );
    }

    #[test]
    fn request_detail_names_what_was_asked_and_what_ran_and_nothing_else() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-switched",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        // Asked for one model, routed to a second, answered by a third.
        monitor.provider_selected("request-switched", "codex", "gpt-5.6-sol", None);
        monitor.model_requested("request-switched", "claude-sonnet-5");
        monitor.model_resolved("request-switched", "gpt-5.6-terra");
        monitor.request_completed("request-switched", 200, Some(10), Some(2));
        let state = monitor.snapshot();

        let detail = buffer_text(&draw(150, 26, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        }));
        assert!(
            detail.contains("requested claude-sonnet-5 · executed gpt-5.6-terra"),
            "{detail}"
        );
        // The routed display projection is a string with an arrow in it, and
        // repeating it here would name the executed model twice.
        assert!(!detail.contains('→'), "{detail}");
        assert_eq!(detail.matches("gpt-5.6-terra").count(), 1, "{detail}");
    }

    #[test]
    fn a_locally_answered_request_never_reads_as_a_call_of_the_model_it_named() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-local",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-local", LOCAL_PROVIDER, "claude-sonnet-5", None);
        monitor.model_requested("request-local", "claude-sonnet-5");
        monitor.request_completed("request-local", 200, Some(1_200), Some(6));
        let state = monitor.snapshot();

        let recent = buffer_text(&draw(154, 6, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                false,
            )
        }));
        assert!(recent.contains("local answer"), "{recent}");
        assert!(!recent.contains("claude-sonnet-5"), "{recent}");

        let detail = buffer_text(&draw(140, 26, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        }));
        assert!(detail.contains("requested claude-sonnet-5"), "{detail}");
        assert!(
            detail.contains("executed none (answered locally)"),
            "{detail}"
        );
    }

    #[test]
    fn token_cells_mark_a_provisional_count_apart_from_an_exact_one() {
        let monitor = MonitorHandle::new(10);
        // Only an opening estimate ever arrived for this one.
        monitor.request_started(
            "request-open",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-open", "codex", "gpt-5.6-sol", None);
        monitor.request_completed("request-open", 200, Some(12_000), Some(30));

        // This one's backend closed its counts itself, a zero output included.
        monitor.request_started(
            "request-exact",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-exact", "codex", "gpt-5.6-sol", None);
        let mut report = UsageReport::default();
        report.closing.input_tokens = Some(8_000);
        report.closing.output_tokens = Some(0);
        monitor.usage_reported("request-exact", report);
        monitor.request_completed("request-exact", 200, None, None);
        let state = monitor.snapshot();

        let recent = buffer_text(&draw(154, 8, |frame| {
            render_recent(
                frame,
                frame.area(),
                &state.recent,
                SelectionView::at(0),
                false,
            )
        }));
        assert!(recent.contains("~12.0k"), "{recent}");
        assert!(recent.contains("8.0k"), "{recent}");
        assert!(!recent.contains("~8.0k"), "{recent}");
        // Every count of both rows has a value, so the exact zero output is
        // the only thing a missing label could be standing in for.
        assert!(!recent.contains(MISSING_TOKENS_LABEL), "{recent}");
    }

    #[test]
    fn an_exact_zero_is_a_number_and_an_unreported_count_is_not() {
        assert_eq!(quality_token_label(Some(0), UsageQuality::Exact), "0");
        assert_eq!(quality_token_label(Some(0), UsageQuality::Opening), "~0");
        assert_eq!(quality_token_label(None, UsageQuality::Missing), "n/a");
        assert_eq!(
            quality_token_label(Some(12_000), UsageQuality::Opening),
            "~12.0k"
        );
    }

    #[test]
    fn a_request_without_a_reported_cache_write_shows_it_unreported_not_zero() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-codex",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-codex", "codex", "gpt-5.6-sol", None);
        monitor.model_resolved("request-codex", "gpt-5.6-sol");
        let mut report = UsageReport::default();
        report.closing.input_tokens = Some(20_000);
        report.closing.cache_read_tokens = Some(5_000);
        report.closing.output_tokens = Some(400);
        monitor.usage_reported("request-codex", report);
        monitor.request_completed("request-codex", 200, None, None);
        let state = monitor.snapshot();

        let detail = buffer_text(&draw(140, 26, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        }));
        assert!(detail.contains("n/a write"), "{detail}");
        assert!(!detail.contains("0 write"), "{detail}");
        assert!(
            detail.contains("parts reported separately: 5m n/a, 1h n/a"),
            "{detail}"
        );
    }

    #[test]
    fn request_detail_breaks_a_cache_write_into_buckets_without_summing_them() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-anthropic",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-anthropic", "anthropic", "claude-sonnet-5", None);
        monitor.model_requested("request-anthropic", "claude-sonnet-5");
        monitor.model_resolved("request-anthropic", "claude-sonnet-5");
        let mut report = UsageReport::default();
        report.closing.input_tokens = Some(1_000);
        report.closing.cache_read_tokens = Some(40_000);
        report.closing.cache_write_tokens = Some(1_200);
        report.closing.cache_write_5m_tokens = Some(800);
        report.closing.cache_write_1h_tokens = Some(400);
        report.closing.output_tokens = Some(120);
        // A total of the backend's own that its own categories do not reach.
        report.reported_prompt_tokens = Some(43_000);
        monitor.usage_reported("request-anthropic", report);
        monitor.request_completed("request-anthropic", 200, None, None);
        let state = monitor.snapshot();

        let detail = buffer_text(&draw(160, 26, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        }));
        assert!(detail.contains("backend total 43.0k"), "{detail}");
        assert!(
            detail.contains("1.0k in · 40.0k read · 1.2k write · 120 out"),
            "{detail}"
        );
        assert!(
            detail.contains("parts reported separately: 5m 800, 1h 400"),
            "{detail}"
        );

        // Cut from the right at a narrow width, the caveat is what survives:
        // the buckets can never be read as a split of the write.
        let narrow = buffer_text(&draw(80, 20, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        }));
        let caveat = narrow.find("parts reported separately");
        match narrow.find("5m ") {
            Some(bucket) => assert!(caveat.is_some_and(|caveat| caveat < bucket), "{narrow}"),
            None => assert!(caveat.is_some(), "{narrow}"),
        }
    }

    #[test]
    fn request_detail_keeps_the_model_and_the_numbers_in_a_short_pane() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-short",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected("request-short", "anthropic", "claude-sonnet-5", None);
        monitor.model_requested("request-short", "claude-sonnet-5");
        monitor.model_resolved("request-short", "claude-sonnet-5");
        let mut report = UsageReport::default();
        report.closing.input_tokens = Some(1_000);
        report.closing.cache_read_tokens = Some(40_000);
        report.closing.cache_write_tokens = Some(1_200);
        report.closing.cache_write_5m_tokens = Some(800);
        report.closing.output_tokens = Some(120);
        monitor.usage_reported("request-short", report);
        monitor.request_completed("request-short", 200, None, None);
        let state = monitor.snapshot();

        // The pane is a share of the terminal and does not scroll, so the
        // model and the counts have to sit above the fold.
        let detail = buffer_text(&draw(150, 16, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        }));
        assert!(detail.contains("requested claude-sonnet-5"), "{detail}");
        assert!(
            detail.contains("42.2k prompt · 1.0k in · 40.0k read"),
            "{detail}"
        );
        assert!(
            detail.contains("parts reported separately: 5m 800, 1h n/a"),
            "{detail}"
        );
        // Every count is exact and only a bucket is missing, which the legend
        // still has to explain.
        assert!(detail.contains("n/a not reported"), "{detail}");
    }

    #[test]
    fn the_prompt_size_is_no_firmer_than_the_counts_it_is_made_of() {
        let monitor = MonitorHandle::new(10);
        // Nothing reported at all: there is no prompt to state.
        monitor.request_started("request-silent", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-silent", "codex", "gpt-5.6-sol", None);
        monitor.request_completed("request-silent", 200, None, None);

        // An estimate of the input makes the sum an estimate too.
        monitor.request_started("request-estimated", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-estimated", "codex", "gpt-5.6-sol", None);
        monitor.request_completed("request-estimated", 200, Some(12_000), Some(30));

        // A total the backend measured itself is exact whatever the split is.
        monitor.request_started("request-total", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-total", "codex", "gpt-5.6-sol", None);
        let report = UsageReport {
            reported_prompt_tokens: Some(9_000),
            ..UsageReport::default()
        };
        monitor.usage_reported("request-total", report);
        monitor.request_completed("request-total", 200, Some(12_000), None);
        let state = monitor.snapshot();

        let detail_of = |request_id: &str| {
            let index = state
                .recent
                .iter()
                .position(|request| request.request_id == request_id)
                .unwrap();
            buffer_text(&draw(150, 26, |frame| {
                render_request_detail(frame, frame.area(), &state, index)
            }))
        };

        let silent = detail_of("request-silent");
        assert!(silent.contains("n/a prompt"), "{silent}");
        let estimated = detail_of("request-estimated");
        assert!(estimated.contains("~12.0k prompt"), "{estimated}");
        let total = detail_of("request-total");
        assert!(total.contains("9.0k prompt"), "{total}");
        assert!(!total.contains("~9.0k prompt"), "{total}");
    }

    #[test]
    fn an_event_row_for_a_locally_answered_request_names_no_model() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-local-failed",
            Some("sess-1".to_string()),
            None,
            EndpointKind::Messages,
        );
        monitor.provider_selected(
            "request-local-failed",
            LOCAL_PROVIDER,
            "claude-sonnet-5",
            None,
        );
        monitor.model_requested("request-local-failed", "claude-sonnet-5");
        monitor.request_failed(
            "request-local-failed",
            Some(500),
            "summary transcript empty",
        );
        let state = monitor.snapshot();

        let events = buffer_text(&draw(154, 8, |frame| {
            render_events(frame, frame.area(), &state.recent)
        }));
        assert!(events.contains("summary transcript empty"), "{events}");
        assert!(events.contains("local answer"), "{events}");
        assert!(!events.contains("claude-sonnet-5"), "{events}");
    }

    #[test]
    fn events_render_matching_request_rows_without_a_placeholder() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &state.recent)
        });
        let events_text = buffer_text(&events);
        assert!(events_text.contains("Time"));
        assert!(events_text.contains("502"));
        assert!(events_text.contains("upstream unavailable"));
        assert!(!events_text.contains("No events"));
    }

    #[test]
    fn shutdown_confirmation_can_be_cancelled_before_signalling_server() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: Selection::default(),
            recent_selected: Selection::default(),
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.request_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::ConfirmingShutdown);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let state = MonitorHandle::default().snapshot();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shut down proxy?"), "{text}");
        assert!(text.contains("y confirm"), "{text}");
        assert!(text.contains("n/Esc/q cancel"), "{text}");

        app.cancel_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::Running);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        app.request_shutdown_confirmation();
        app.begin_shutdown();
        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
    }

    #[test]
    fn ctrl_c_starts_shutdown_then_requests_force_quit() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (_shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel();
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: Selection::default(),
            recent_selected: Selection::default(),
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(shutdown_complete_rx),
        };

        assert!(!app.handle_ctrl_c());
        assert!(app.handle_ctrl_c());

        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
        let state = MonitorHandle::default().snapshot();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shutting down..."));
        assert!(text.contains("Press Ctrl-C to force quit"));
    }

    #[test]
    fn shutdown_completion_accepts_notification_and_sender_drop() {
        let (complete_tx, complete_rx) = mpsc::channel();
        let app = MonitorApp {
            listen_url: String::new(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: Selection::default(),
            recent_selected: Selection::default(),
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(complete_rx),
        };

        assert!(!app.shutdown_is_complete());
        complete_tx.send(()).unwrap();
        assert!(app.shutdown_is_complete());

        let (complete_tx, complete_rx) = mpsc::channel();
        let mut app = app;
        app.shutdown_complete = Some(complete_rx);
        drop(complete_tx);
        assert!(app.shutdown_is_complete());
    }

    #[test]
    fn header_renders_configured_listen_url() {
        let app = MonitorApp {
            listen_url: "http://[::]:18765".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: Selection::default(),
            recent_selected: Selection::default(),
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };
        let state = MonitorHandle::default().snapshot();

        let header = draw(100, 1, |frame| {
            render_header(frame, frame.area(), &app, &state)
        });

        assert!(buffer_text(&header).contains("http://[::]:18765"));
    }

    /// Two session rows and three requests to navigate over.
    fn navigable_state() -> MonitorState {
        let monitor = MonitorHandle::new(10);
        record_session_request(&monitor, "request-1", "sess-1", "project-1");
        record_session_request(&monitor, "request-2", "sess-2", "project-2");
        record_session_request(&monitor, "request-3", "sess-2", "project-2");
        monitor.snapshot()
    }

    #[test]
    fn a_selection_never_points_past_the_rows_a_snapshot_has() {
        let state = navigable_state();
        let rows = session_rows(&state.sessions);
        let mut app = monitor_app(FocusPane::Sessions);
        app.sync_selection(&rows, &state.recent);
        app.move_down(&rows, &state.recent, false);
        assert_eq!(app.selected.row(), Some(1));
        assert_eq!(app.recent_selected.row(), Some(0));

        // A snapshot holding neither of those rows leaves both panes pointing
        // at nothing rather than at a row number nothing answers for.
        let empty = MonitorHandle::new(10).snapshot();
        app.sync_selection(&session_rows(&empty.sessions), &empty.recent);
        assert_eq!(app.selected.row(), None);
        assert_eq!(app.recent_selected.row(), None);
    }

    #[test]
    fn arrow_navigation_moves_between_focus_panes_at_edges() {
        let state = navigable_state();
        let rows = session_rows(&state.sessions);
        let mut app = monitor_app(FocusPane::Sessions);
        app.sync_selection(&rows, &state.recent);

        app.move_down(&rows, &state.recent, true);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected.row(), Some(1));

        app.move_down(&rows, &state.recent, true);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected.row(), Some(0));

        app.move_up(&rows, &state.recent, true);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected.row(), Some(1));
    }

    #[test]
    fn vim_navigation_stays_within_focused_pane() {
        let state = navigable_state();
        let rows = session_rows(&state.sessions);
        let mut app = monitor_app(FocusPane::Sessions);
        app.sync_selection(&rows, &state.recent);
        app.move_down(&rows, &state.recent, false);

        app.move_down(&rows, &state.recent, false);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected.row(), Some(1));

        app.focus = FocusPane::Recent;
        app.move_up(&rows, &state.recent, false);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected.row(), Some(0));
    }
}
