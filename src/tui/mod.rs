//! Terminal UI: one status line, the selected view, one key-hint footer.
//! Renders from a polled [`StoreView`] and, when embedded in the `local`
//! supervisor, a live [`EngineStatus`] feed plus the captured log.

pub mod data;
pub mod feed;
pub mod log;
mod views;
mod widgets;

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    Error, Result,
    config::Settings,
    domain::{RecommendationClass, ResearchOpportunity, Sport},
    paper,
    storage::Store,
};
use data::{StoreView, spawn_store_poller};
use feed::{EngineCommand, EngineHandle, EngineStatus};
use log::LogBuffer;
use widgets::Tone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Overview,
    Markets,
    Sources,
    Portfolio,
    System,
}

impl View {
    pub const ALL: [View; 5] = [
        View::Overview,
        View::Markets,
        View::Sources,
        View::Portfolio,
        View::System,
    ];

    fn label(self) -> &'static str {
        match self {
            View::Overview => "Overview",
            View::Markets => "Markets",
            View::Sources => "Sources",
            View::Portfolio => "Portfolio",
            View::System => "System",
        }
    }

    fn shortcut(self) -> char {
        match self {
            View::Overview => '1',
            View::Markets => '2',
            View::Sources => '3',
            View::Portfolio => '4',
            View::System => '5',
        }
    }

    fn next(self) -> View {
        let index = View::ALL.iter().position(|view| *view == self).unwrap_or(0);
        View::ALL[(index + 1) % View::ALL.len()]
    }

    fn previous(self) -> View {
        let index = View::ALL.iter().position(|view| *view == self).unwrap_or(0);
        View::ALL[(index + View::ALL.len() - 1) % View::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    NetEdge,
    RawEdge,
    Families,
    Class,
    Sport,
}

impl SortKey {
    fn next(self) -> SortKey {
        match self {
            SortKey::NetEdge => SortKey::RawEdge,
            SortKey::RawEdge => SortKey::Families,
            SortKey::Families => SortKey::Class,
            SortKey::Class => SortKey::Sport,
            SortKey::Sport => SortKey::NetEdge,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortKey::NetEdge => "net edge",
            SortKey::RawEdge => "raw edge",
            SortKey::Families => "families",
            SortKey::Class => "class",
            SortKey::Sport => "sport",
        }
    }
}

/// Transient one-line message shown in the status bar.
#[derive(Debug, Clone)]
pub struct Notice {
    pub at: chrono::DateTime<Utc>,
    pub text: String,
    pub tone: Tone,
}

pub struct App {
    pub settings: Settings,
    pub view: View,
    pub store: Arc<StoreView>,
    pub engine: Option<EngineStatus>,
    pub logs: Option<LogBuffer>,
    pub sort: SortKey,
    pub sport_filter: Option<Sport>,
    pub markets_cursor: usize,
    pub show_detail: bool,
    pub portfolio_cursor: usize,
    pub log_scroll: usize,
    pub notice: Option<Notice>,
    pub tick: u64,
    pub attached: bool,
}

impl App {
    /// Rows for the Markets view under the current filter and sort.
    pub fn market_rows(&self) -> Vec<&ResearchOpportunity> {
        let mut rows = self
            .store
            .rows
            .iter()
            .filter(|row| {
                self.sport_filter
                    .is_none_or(|sport| row.opportunity.sport == sport)
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            let (l, r) = (&left.opportunity, &right.opportunity);
            match self.sort {
                SortKey::NetEdge => r.net_edge.cmp(&l.net_edge),
                SortKey::RawEdge => r.raw_edge.cmp(&l.raw_edge),
                SortKey::Families => r
                    .family_count
                    .cmp(&l.family_count)
                    .then(r.raw_edge.cmp(&l.raw_edge)),
                SortKey::Class => class_rank(left.effective_class)
                    .cmp(&class_rank(right.effective_class))
                    .then(r.net_edge.cmp(&l.net_edge)),
                SortKey::Sport => l
                    .sport
                    .to_string()
                    .cmp(&r.sport.to_string())
                    .then(r.net_edge.cmp(&l.net_edge)),
            }
        });
        rows
    }

    /// Candidates first, then the closest misses, for the Overview table.
    pub fn candidate_rows(&self) -> Vec<&ResearchOpportunity> {
        let mut rows = self.store.rows.iter().collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            class_rank(left.effective_class)
                .cmp(&class_rank(right.effective_class))
                .then(
                    right
                        .opportunity
                        .family_count
                        .cmp(&left.opportunity.family_count),
                )
                .then(right.opportunity.raw_edge.cmp(&left.opportunity.raw_edge))
        });
        rows
    }

    pub fn selected_market(&self) -> Option<&ResearchOpportunity> {
        let rows = self.market_rows();
        rows.get(self.markets_cursor).copied()
    }

    fn notify(&mut self, text: impl Into<String>, tone: Tone) {
        self.notice = Some(Notice {
            at: Utc::now(),
            text: text.into(),
            tone,
        });
    }
}

pub fn class_rank(class: RecommendationClass) -> u8 {
    match class {
        RecommendationClass::Actionable => 0,
        RecommendationClass::Watchlist => 1,
        RecommendationClass::Rejected => 2,
    }
}

pub struct TuiOptions {
    pub settings: Settings,
    pub store: Arc<dyn Store>,
    pub engine: Option<EngineHandle>,
    pub logs: Option<LogBuffer>,
}

/// Own the terminal until the user quits. With an engine handle, `q` also
/// requests engine shutdown and `r` triggers an immediate scan.
pub async fn run(options: TuiOptions) -> Result<()> {
    let TuiOptions {
        settings,
        store,
        engine,
        logs,
    } = options;
    let (refresh_tx, refresh_rx) = watch::channel(0u64);
    let mut store_rx = spawn_store_poller(Arc::clone(&store), settings.clone(), refresh_rx);
    let mut engine_rx = engine.as_ref().map(|handle| handle.status.clone());
    let mut app = App {
        settings: settings.clone(),
        view: View::Overview,
        store: store_rx.borrow().clone(),
        engine: engine_rx.as_ref().map(|rx| rx.borrow().clone()),
        logs,
        sort: SortKey::NetEdge,
        sport_filter: None,
        markets_cursor: 0,
        show_detail: true,
        portfolio_cursor: 0,
        log_scroll: 0,
        notice: None,
        tick: 0,
        attached: engine.is_none(),
    };

    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut store_rx,
        engine_rx.as_mut(),
        engine.as_ref(),
        store.as_ref(),
        &refresh_tx,
    )
    .await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    store_rx: &mut watch::Receiver<Arc<StoreView>>,
    mut engine_rx: Option<&mut watch::Receiver<EngineStatus>>,
    engine: Option<&EngineHandle>,
    store: &dyn Store,
    refresh_tx: &watch::Sender<u64>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut needs_draw = true;
    loop {
        if needs_draw {
            terminal
                .draw(|frame| views::render(frame, app))
                .map_err(|error| Error::Config(format!("terminal draw: {error}")))?;
            needs_draw = false;
        }
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else { return Ok(()); };
                let event = event.map_err(|error| Error::Config(format!("terminal event: {error}")))?;
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        match handle_key(app, key, engine, store, refresh_tx).await {
                            Outcome::Quit => return Ok(()),
                            Outcome::Redraw => needs_draw = true,
                            Outcome::Ignored => {}
                        }
                    }
                    Event::Resize(_, _) => needs_draw = true,
                    _ => {}
                }
            }
            changed = store_rx.changed() => {
                if changed.is_ok() {
                    app.store = store_rx.borrow_and_update().clone();
                    needs_draw = true;
                }
            }
            changed = async {
                match engine_rx.as_mut() {
                    Some(rx) => rx.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => {
                if changed && let Some(rx) = engine_rx.as_mut() {
                    app.engine = Some(rx.borrow_and_update().clone());
                    needs_draw = true;
                }
            }
            _ = clock.tick() => {
                app.tick += 1;
                if app
                    .notice
                    .as_ref()
                    .is_some_and(|notice| (Utc::now() - notice.at).num_seconds() > 8)
                {
                    app.notice = None;
                }
                needs_draw = true;
            }
        }
    }
}

enum Outcome {
    Ignored,
    Redraw,
    Quit,
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    engine: Option<&EngineHandle>,
    store: &dyn Store,
    refresh_tx: &watch::Sender<u64>,
) -> Outcome {
    let page = 10usize;
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if let Some(engine) = engine {
                let _ = engine.commands.send(EngineCommand::Shutdown);
            }
            Outcome::Quit
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(engine) = engine {
                let _ = engine.commands.send(EngineCommand::Shutdown);
            }
            Outcome::Quit
        }
        KeyCode::Tab => {
            app.view = app.view.next();
            Outcome::Redraw
        }
        KeyCode::BackTab => {
            app.view = app.view.previous();
            Outcome::Redraw
        }
        KeyCode::Char(digit @ '1'..='5') => {
            app.view = View::ALL[(digit as u8 - b'1') as usize];
            Outcome::Redraw
        }
        KeyCode::Char('r') => match engine {
            Some(engine) => {
                let _ = engine.commands.send(EngineCommand::ScanNow);
                app.notify("scan requested", Tone::Accent);
                Outcome::Redraw
            }
            None => {
                let _ = refresh_tx.send(refresh_tx.borrow().wrapping_add(1));
                app.notify("store reloaded", Tone::Accent);
                Outcome::Redraw
            }
        },
        KeyCode::Char('s') if app.view == View::Markets => {
            app.sort = app.sort.next();
            app.markets_cursor = 0;
            Outcome::Redraw
        }
        KeyCode::Char('f') if app.view == View::Markets => {
            app.sport_filter = next_sport_filter(app.sport_filter);
            app.markets_cursor = 0;
            Outcome::Redraw
        }
        KeyCode::Enter if app.view == View::Markets => {
            app.show_detail = !app.show_detail;
            Outcome::Redraw
        }
        KeyCode::Char('o') if matches!(app.view, View::Markets | View::Overview) => {
            let selected = match app.view {
                View::Markets => app.selected_market().map(|row| row.opportunity.id),
                _ => app.candidate_rows().first().map(|row| row.opportunity.id),
            };
            match selected {
                Some(id) => {
                    paper_action(app, store, refresh_tx, PaperAction::Open(id)).await;
                    Outcome::Redraw
                }
                None => Outcome::Ignored,
            }
        }
        KeyCode::Char('c') if app.view == View::Portfolio => {
            let selected = app
                .store
                .portfolio
                .open_positions
                .get(app.portfolio_cursor)
                .map(|position| position.opportunity_id);
            match selected {
                Some(id) => {
                    paper_action(app, store, refresh_tx, PaperAction::Close(id)).await;
                    Outcome::Redraw
                }
                None => Outcome::Ignored,
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_cursor(app, -1);
            Outcome::Redraw
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_cursor(app, 1);
            Outcome::Redraw
        }
        KeyCode::PageUp => {
            move_cursor(app, -(page as isize));
            Outcome::Redraw
        }
        KeyCode::PageDown => {
            move_cursor(app, page as isize);
            Outcome::Redraw
        }
        KeyCode::Home | KeyCode::Char('g') => {
            set_cursor(app, 0);
            Outcome::Redraw
        }
        KeyCode::End | KeyCode::Char('G') => {
            set_cursor(app, usize::MAX);
            Outcome::Redraw
        }
        _ => Outcome::Ignored,
    }
}

fn next_sport_filter(current: Option<Sport>) -> Option<Sport> {
    match current {
        None => Some(Sport::ALL[0]),
        Some(sport) => {
            let index = Sport::ALL
                .iter()
                .position(|item| *item == sport)
                .unwrap_or(0);
            Sport::ALL.get(index + 1).copied()
        }
    }
}

fn cursor_bounds(app: &App) -> (usize, usize) {
    match app.view {
        View::Markets => (app.markets_cursor, app.market_rows().len()),
        View::Portfolio => (
            app.portfolio_cursor,
            app.store.portfolio.open_positions.len(),
        ),
        View::System => (
            app.log_scroll,
            app.logs.as_ref().map(|logs| logs.len()).unwrap_or(0),
        ),
        _ => (0, 0),
    }
}

fn move_cursor(app: &mut App, delta: isize) {
    let (current, len) = cursor_bounds(app);
    if len == 0 {
        return;
    }
    let next = (current as isize + delta).clamp(0, len as isize - 1) as usize;
    set_cursor(app, next);
}

fn set_cursor(app: &mut App, value: usize) {
    let (_, len) = cursor_bounds(app);
    let value = value.min(len.saturating_sub(1));
    match app.view {
        View::Markets => app.markets_cursor = value,
        View::Portfolio => app.portfolio_cursor = value,
        View::System => app.log_scroll = value,
        _ => {}
    }
}

enum PaperAction {
    Open(Uuid),
    Close(Uuid),
}

async fn paper_action(
    app: &mut App,
    store: &dyn Store,
    refresh_tx: &watch::Sender<u64>,
    action: PaperAction,
) {
    let result = match action {
        PaperAction::Open(id) => {
            paper::open_position(store, &app.settings, id)
                .await
                .map(|portfolio| {
                    format!(
                        "paper position opened; exposure {}",
                        portfolio.open_exposure
                    )
                })
        }
        PaperAction::Close(id) => {
            paper::close_position(store, &app.settings, id)
                .await
                .map(|portfolio| {
                    format!(
                        "paper position closed; exposure {}",
                        portfolio.open_exposure
                    )
                })
        }
    };
    match result {
        Ok(message) => app.notify(message, Tone::Good),
        Err(error) => app.notify(error.to_string(), Tone::Bad),
    }
    let _ = refresh_tx.send(refresh_tx.borrow().wrapping_add(1));
}
