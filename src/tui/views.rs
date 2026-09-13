//! View renderers. Layout: status line · body · footer.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table, Wrap},
};

use super::{App, View, widgets::*};
use crate::{
    domain::{PaperPosition, RecommendationClass, ResearchOpportunity, Sport},
    tui::{
        data::{GateGap, decimal_f64},
        feed::Phase,
    },
};

pub(super) fn render(frame: &mut Frame<'_>, app: &App) {
    let [status_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_status(frame, status_area, app);
    render_footer(frame, footer_area, app);
    match app.view {
        View::Overview => render_overview(frame, body_area, app),
        View::Markets => render_markets(frame, body_area, app),
        View::Sources => render_sources(frame, body_area, app),
        View::Portfolio => render_portfolio(frame, body_area, app),
        View::System => render_system(frame, body_area, app),
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("polybot", Tone::Neutral.bold()),
        sep(),
        badge(
            format!("{:?}", app.settings.run_mode).to_ascii_uppercase(),
            Tone::Accent,
        ),
        sep(),
    ];
    if app.attached {
        spans.push(badge("ATTACHED", Tone::Muted));
    } else if let Some(engine) = &app.engine {
        let (text, tone) = match &engine.phase {
            Phase::Starting => ("STARTING".to_string(), Tone::Warn),
            Phase::Idle => ("IDLE".to_string(), Tone::Good),
            Phase::Scanning { since } => (format!("SCANNING {}", age(Some(*since))), Tone::Accent),
            Phase::Reviewing => ("REVIEWING".to_string(), Tone::Accent),
            Phase::ShuttingDown => ("SHUTDOWN".to_string(), Tone::Bad),
        };
        spans.push(badge(text, tone));
    }
    let scan = app
        .engine
        .as_ref()
        .and_then(|engine| engine.last_scan.as_ref())
        .or(app.store.scan.as_ref());
    spans.push(Span::raw("   "));
    spans.push(label("last scan"));
    spans.push(plain(age(scan.map(|scan| scan.completed_at))));
    if let Some(scan) = scan {
        spans.push(sep());
        spans.push(label("took"));
        spans.push(plain(millis_label(
            (scan.completed_at - scan.started_at)
                .num_milliseconds()
                .max(0) as u64,
        )));
        spans.push(sep());
        spans.push(plain(format!(
            "{} markets · {} quotes · {} evaluated · ",
            scan.market_count, scan.quote_count, scan.evaluated_count
        )));
        let tone = if scan.candidate_count > 0 {
            Tone::Good
        } else {
            Tone::Neutral
        };
        spans.push(Span::styled(
            format!("{} candidates", scan.candidate_count),
            tone.bold(),
        ));
    }
    if let Some(engine) = &app.engine {
        spans.push(sep());
        spans.push(label("next"));
        spans.push(plain(countdown(engine.next_scan_at)));
    }
    if let Some(notice) = &app.notice {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(notice.text.clone(), notice.tone.bold()));
    } else if let Some(error) = app.store.error.as_deref().or(app
        .engine
        .as_ref()
        .and_then(|engine| engine.last_error.as_deref()))
    {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("error: {}", fit(error, 60)),
            Tone::Bad.style(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let view_hints: &[(&str, &str)] = match app.view {
        View::Overview => &[("o", "paper open top"), ("r", "rescan")],
        View::Markets => &[
            ("↑↓/jk", "row"),
            ("s", "sort"),
            ("f", "sport"),
            ("⏎", "detail"),
            ("o", "paper open"),
        ],
        View::Sources => &[("r", "rescan")],
        View::Portfolio => &[("↑↓/jk", "row"), ("c", "close")],
        View::System => &[("↑↓/jk", "log"), ("g/G", "newest/oldest")],
    };
    let mut hints = view_hints.to_vec();
    hints.extend([("Tab/1-5", "views"), ("q", "quit")]);
    let line = key_hints(&hints);
    let tabs = view_tabs(app.view);
    let tabs_width = tabs.width() as u16;
    let [hints_area, tabs_area] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(tabs_width + 1)]).areas(area);
    frame.render_widget(Paragraph::new(line), hints_area);
    frame.render_widget(Paragraph::new(tabs).alignment(Alignment::Right), tabs_area);
}

fn view_tabs(selected: View) -> Line<'static> {
    let mut spans = Vec::with_capacity(View::ALL.len());
    for view in View::ALL {
        let text = format!("{} {}", view.shortcut(), view.label());
        if view == selected {
            spans.push(Span::styled(format!("[{text}]"), Tone::Warn.bold()));
        } else {
            spans.push(Span::styled(format!(" {text} "), Tone::Muted.style()));
        }
    }
    Line::from(spans)
}

// ---------------------------------------------------------------- overview

fn render_overview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    const PANEL_HEIGHT: u16 = 8;
    const MIN_LOG: u16 = 4;
    let has_log = app.logs.is_some();
    let constraints = if has_log {
        vec![
            Constraint::Length(PANEL_HEIGHT),
            Constraint::Min(6),
            Constraint::Length(MIN_LOG + 3),
        ]
    } else {
        vec![Constraint::Length(PANEL_HEIGHT), Constraint::Min(6)]
    };
    let areas = Layout::vertical(constraints).split(area);
    let [scan_area, sources_area, edge_area] = Layout::horizontal([
        Constraint::Length(34),
        Constraint::Min(30),
        Constraint::Length(46),
    ])
    .spacing(1)
    .areas(areas[0]);
    render_scan_panel(frame, scan_area, app);
    render_sources_panel(frame, sources_area, app);
    render_edge_panel(frame, edge_area, app);
    render_candidates(frame, areas[1], app);
    if has_log && areas.len() > 2 {
        render_log(frame, areas[2], app, MIN_LOG as usize + 2, 0);
    }
}

fn render_scan_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let inner = section(frame, area, "SCAN", Vec::new());
    let mut lines = Vec::new();
    let scan = app
        .engine
        .as_ref()
        .and_then(|engine| engine.last_scan.as_ref())
        .or(app.store.scan.as_ref());
    match scan {
        Some(scan) => {
            lines.push(Line::from(vec![
                label("completed"),
                plain(clock(scan.completed_at)),
                muted(format!(" ({} ago)", age(Some(scan.completed_at)))),
            ]));
            lines.push(Line::from(vec![
                label("duration "),
                plain(millis_label(
                    (scan.completed_at - scan.started_at)
                        .num_milliseconds()
                        .max(0) as u64,
                )),
            ]));
            lines.push(Line::from(vec![
                label("markets  "),
                plain(scan.market_count.to_string()),
                sep(),
                label("quotes"),
                plain(scan.quote_count.to_string()),
            ]));
            lines.push(Line::from(vec![
                label("evaluated"),
                plain(scan.evaluated_count.to_string()),
                sep(),
                label("candidates"),
                Span::styled(
                    scan.candidate_count.to_string(),
                    if scan.candidate_count > 0 {
                        Tone::Good.bold()
                    } else {
                        Tone::Neutral.style()
                    },
                ),
            ]));
        }
        None => lines.push(Line::from(muted("no scan recorded yet"))),
    }
    if let Some(engine) = &app.engine {
        lines.push(Line::from(vec![
            label("scans    "),
            plain(format!("{} ok", engine.scans_completed)),
            sep(),
            Span::styled(
                format!("{} failed", engine.scans_failed),
                if engine.scans_failed > 0 {
                    Tone::Bad.style()
                } else {
                    Tone::Muted.style()
                },
            ),
        ]));
        lines.push(Line::from(vec![
            label("interval "),
            plain(duration_label(app.settings.scan_interval.as_secs() as i64)),
            sep(),
            label("next"),
            plain(countdown(engine.next_scan_at)),
        ]));
        lines.push(Line::from(vec![
            label("news     "),
            plain(engine.reviewer.unwrap_or("none")),
            sep(),
            plain(match engine.last_news_pass {
                Some((at, reviewed)) => format!("{reviewed} reviewed {} ago", age(Some(at))),
                None => "no pass yet".into(),
            }),
        ]));
    } else {
        lines.push(Line::from(vec![
            label("store    "),
            plain(format!("polled {} ago", age(Some(app.store.loaded_at)))),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_sources_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let scan = app
        .engine
        .as_ref()
        .and_then(|engine| engine.last_scan.as_ref())
        .or(app.store.scan.as_ref());
    let health = scan
        .map(|scan| scan.source_health.as_slice())
        .unwrap_or(&[]);
    let reachable = health.iter().filter(|item| item.reachable).count();
    let tone = if health.is_empty() {
        Tone::Muted
    } else if reachable == health.len() {
        Tone::Good
    } else {
        Tone::Warn
    };
    let inner = section(
        frame,
        area,
        "SOURCES",
        vec![Span::styled(
            format!("{reachable}/{} reachable", health.len()),
            tone.style(),
        )],
    );
    let rows = health.iter().map(|item| {
        let (mark, tone) = if item.reachable {
            ("●", Tone::Good)
        } else {
            ("○", Tone::Bad)
        };
        Row::new(vec![
            Cell::from(Span::styled(mark, tone.bold())),
            Cell::from(item.source_id.clone()),
            Cell::from(Line::from(item.odds_found.to_string()).alignment(Alignment::Right)),
            Cell::from(Line::from(millis_label(item.latency_ms)).alignment(Alignment::Right)),
            Cell::from(muted(fit(&item.message, 28))),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(18),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Min(8),
        ],
    )
    .header(Row::new(vec!["", "source", "quotes", "latency", "status"]).style(Tone::Muted.style()))
    .column_spacing(1);
    frame.render_widget(table, inner);
}

fn render_edge_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let stats = &app.store.stats;
    let inner = section(frame, area, "EDGE & PAPER", Vec::new());
    let total = stats.evaluated.max(1) as f64;
    let mut lines = vec![Line::from(vec![
        badge(format!("{} actionable", stats.actionable), Tone::Good),
        sep(),
        badge(format!("{} watchlist", stats.watchlist), Tone::Warn),
        sep(),
        muted(format!("{} rejected", stats.rejected)),
    ])];
    for (index, label_text) in ["1 family ", "2 families", "3+ families"]
        .iter()
        .enumerate()
    {
        let count = stats.by_families[index];
        let mut spans = vec![label(label_text)];
        spans.extend(gauge(count as f64, total, 12, Tone::Accent));
        spans.push(plain(format!(" {count:>3}")));
        lines.push(Line::from(spans));
    }
    match (stats.mean_raw_edge, stats.sd_raw_edge, stats.best_raw_edge) {
        (Some(mean), Some(sd), Some(best)) => lines.push(Line::from(vec![
            label("quorum edge"),
            signed_pp(mean),
            muted(format!(" ±{:.2}pp", sd * 100.0)),
            sep(),
            label("best"),
            signed_pp(best),
        ])),
        _ => lines.push(Line::from(muted("no market has three families yet"))),
    }
    let portfolio = &app.store.portfolio;
    let exposure = decimal_f64(portfolio.open_exposure);
    let cap = decimal_f64(app.settings.maximum_total_exposure);
    let mut spans = vec![
        label("paper"),
        plain(format!("{} of {} exposed ", usd(exposure), usd(cap))),
    ];
    spans.extend(gauge(
        exposure,
        cap,
        8,
        if exposure >= cap {
            Tone::Bad
        } else {
            Tone::Good
        },
    ));
    spans.push(plain(format!(" {} open", portfolio.open_positions.len())));
    lines.push(Line::from(spans));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_candidates(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = app.candidate_rows();
    let candidates = rows
        .iter()
        .filter(|row| row.effective_class != RecommendationClass::Rejected)
        .count();
    let detail = if candidates > 0 {
        vec![Span::styled(
            format!("{candidates} live"),
            Tone::Good.style(),
        )]
    } else {
        vec![muted(format!(
            "none live · nearest the gates first (raw ≥ {}, net ≥ {}, {}+ families, price {}–{}) · gap = what each row still needs",
            app.settings.minimum_raw_edge,
            app.settings.minimum_net_edge,
            app.settings.watchlist_source_families,
            app.settings.minimum_price,
            app.settings.maximum_price,
        ))]
    };
    let inner = section(frame, area, "CANDIDATES", detail);
    render_market_table(frame, inner, app, &rows, None);
}

// ---------------------------------------------------------------- markets

fn render_markets(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = app.market_rows();
    let mut detail = vec![
        muted(format!("{} rows", rows.len())),
        sep(),
        label("sort"),
        plain(app.sort.label()),
        sep(),
        label("sport"),
        plain(
            app.sport_filter
                .map(|sport| sport.to_string().to_ascii_uppercase())
                .unwrap_or_else(|| "all".into()),
        ),
    ];
    if !rows.is_empty() {
        detail.push(sep());
        detail.push(muted(format!(
            "row {} of {}",
            app.markets_cursor + 1,
            rows.len()
        )));
    }
    let (table_area, detail_area) = if app.show_detail && !rows.is_empty() {
        let [table_area, detail_area] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(8)]).areas(area);
        (table_area, Some(detail_area))
    } else {
        (area, None)
    };
    let inner = section(frame, table_area, "MARKETS", detail);
    render_market_table(frame, inner, app, &rows, Some(app.markets_cursor));
    if let (Some(detail_area), Some(row)) = (detail_area, rows.get(app.markets_cursor)) {
        render_market_detail(frame, detail_area, row);
    }
}

fn render_market_table(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    rows: &[&ResearchOpportunity],
    cursor: Option<usize>,
) {
    let visible = usize::from(area.height.saturating_sub(1)).max(1);
    let start = cursor
        .map(|cursor| cursor.saturating_sub(visible.saturating_sub(1)).min(cursor))
        .unwrap_or(0);
    let start = if let Some(cursor) = cursor {
        // Keep the cursor inside the viewport with minimal scrolling.
        if cursor < start + visible {
            start
        } else {
            cursor + 1 - visible
        }
    } else {
        0
    };
    let table_rows = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, row)| {
            let o = &row.opportunity;
            let class = row.effective_class;
            let news = row
                .news
                .as_ref()
                .map(|news| {
                    Span::styled(
                        news.confidence_effect.clone(),
                        Tone::for_effect(&news.confidence_effect).style(),
                    )
                })
                .unwrap_or_else(|| {
                    if class == RecommendationClass::Rejected {
                        muted("-")
                    } else {
                        muted("pending")
                    }
                });
            let raw = decimal_f64(o.raw_edge);
            let net = decimal_f64(o.net_edge);
            let gap = GateGap::of(row, &app.settings);
            let gap_cell = if gap.failures() == 0 {
                muted("-")
            } else {
                Span::styled(gap.label(), Tone::Warn.style())
            };
            let mut table_row = Row::new(vec![
                Cell::from(badge(class_label(class), Tone::for_class(class))),
                Cell::from(sport_label(o.sport)),
                Cell::from(fit(&o.participant, 24)),
                Cell::from(format!("{:?}", o.side).to_ascii_lowercase()),
                Cell::from(muted(countdown(Some(o.start_time)).replace("in ", ""))),
                Cell::from(right(pct(decimal_f64(o.fair_probability)))),
                Cell::from(right(price(decimal_f64(o.executable_price)))),
                Cell::from(Line::from(signed_pp(raw)).alignment(Alignment::Right)),
                Cell::from(Line::from(signed_pp(net)).alignment(Alignment::Right)),
                Cell::from(right(o.family_count.to_string())),
                Cell::from(gap_cell),
                Cell::from(muted(fit(&o.source_ids.join(","), 34))),
                Cell::from(news),
            ]);
            if cursor == Some(index) {
                table_row = table_row.style(selected_row_style());
            }
            table_row
        });
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(24),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(3),
            Constraint::Length(26),
            Constraint::Min(12),
            Constraint::Length(9),
        ],
    )
    .header(
        Row::new(vec![
            "class",
            "sport",
            "participant",
            "side",
            "starts",
            "fair",
            "exec",
            "raw",
            "net",
            "fam",
            "gap to gates",
            "sources",
            "news",
        ])
        .style(Tone::Muted.style()),
    )
    .column_spacing(1);
    frame.render_widget(table, area);
}

fn render_market_detail(frame: &mut Frame<'_>, area: Rect, row: &ResearchOpportunity) {
    let o = &row.opportunity;
    let inner = section(
        frame,
        area,
        "DETAIL",
        vec![muted(o.market_slug.clone()), sep(), muted(o.id.to_string())],
    );
    let mut lines = vec![
        Line::from(vec![
            label("fair"),
            plain(pct(decimal_f64(o.fair_probability))),
            sep(),
            label("conservative"),
            plain(pct(decimal_f64(o.conservative_probability))),
            sep(),
            label("executable"),
            plain(price(decimal_f64(o.executable_price))),
            sep(),
            label("maker"),
            plain(
                o.maker_price
                    .map(|value| price(decimal_f64(value)))
                    .unwrap_or_else(|| "-".into()),
            ),
            sep(),
            label("maker edge"),
            o.maker_net_edge
                .map(|value| signed_pp(decimal_f64(value)))
                .unwrap_or_else(|| muted("-")),
        ]),
        Line::from(vec![
            label("quantity"),
            plain(format!("{}", o.quantity.round_dp(2))),
            sep(),
            label("max loss"),
            plain(usd(decimal_f64(o.maximum_loss))),
            sep(),
            label("fee"),
            plain(usd(decimal_f64(o.estimated_fee))),
            sep(),
            label("starts"),
            plain(format!(
                "{} ({})",
                o.start_time.format("%a %H:%MZ"),
                countdown(Some(o.start_time))
            )),
            sep(),
            label("book moved"),
            plain(format!("{} ago", age(Some(o.book_time)))),
            sep(),
            label("sources"),
            plain(o.source_ids.join(", ")),
        ]),
    ];
    if let Some(news) = &row.news {
        lines.push(Line::from(vec![
            label("news"),
            badge(
                news.confidence_effect.clone(),
                Tone::for_effect(&news.confidence_effect),
            ),
            plain(format!(
                " {}",
                fit(&news.summary, usize::from(inner.width).saturating_sub(16))
            )),
        ]));
    }
    if o.reasons.is_empty() {
        lines.push(Line::from(Span::styled(
            "passes every gate",
            Tone::Good.style(),
        )));
    } else {
        lines.push(Line::from(vec![
            label("why not"),
            Span::styled(o.reasons.join(" · "), Tone::Warn.style()),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

// ---------------------------------------------------------------- sources

fn render_sources(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [health_area, coverage_area] =
        Layout::vertical([Constraint::Length(9), Constraint::Min(6)]).areas(area);
    render_sources_panel(frame, health_area, app);

    // Coverage: how many evaluated markets each source touched, per sport.
    let mut coverage = std::collections::BTreeMap::<String, [usize; 5]>::new();
    let mut seen = std::collections::HashSet::new();
    for row in &app.store.rows {
        let o = &row.opportunity;
        if !seen.insert(o.market_id.clone()) {
            continue;
        }
        let sport_index = Sport::ALL
            .iter()
            .position(|sport| *sport == o.sport)
            .unwrap_or(0);
        for source in &o.source_ids {
            coverage.entry(source.clone()).or_default()[sport_index] += 1;
        }
    }
    let inner = section(
        frame,
        coverage_area,
        "COVERAGE",
        vec![muted("markets matched per source and sport (latest scan)")],
    );
    let rows = coverage.iter().map(|(source, counts)| {
        let total: usize = counts.iter().sum();
        let mut cells = vec![Cell::from(source.clone())];
        cells.extend(
            counts
                .iter()
                .map(|count| Cell::from(right(count.to_string()))),
        );
        cells.push(Cell::from(
            Line::from(Span::styled(total.to_string(), Tone::Neutral.bold()))
                .alignment(Alignment::Right),
        ));
        Row::new(cells)
    });
    let mut header = vec!["source".to_string()];
    header.extend(
        Sport::ALL
            .iter()
            .map(|sport| sport.to_string().to_ascii_uppercase()),
    );
    header.push("total".into());
    let table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
        ],
    )
    .header(Row::new(header).style(Tone::Muted.style()))
    .column_spacing(1);
    frame.render_widget(table, inner);
}

// ---------------------------------------------------------------- portfolio

fn render_portfolio(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let portfolio = &app.store.portfolio;
    let exposure = decimal_f64(portfolio.open_exposure);
    let cap = decimal_f64(app.settings.maximum_total_exposure);
    let realized = decimal_f64(portfolio.realized_pnl);
    // Closed positions take the rows they need (up to eight) below the open
    // book; the open book keeps the rest.
    let closed_height = match portfolio.closed_positions.len() {
        0 => 0,
        count => count.min(8) as u16 + 2,
    };
    let [summary_area, open_area, closed_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(closed_height),
    ])
    .areas(area);
    let inner = section(frame, summary_area, "PAPER PORTFOLIO", Vec::new());
    let mut gauge_spans = vec![
        label("exposure"),
        plain(format!("{} / {} ", usd(exposure), usd(cap))),
    ];
    gauge_spans.extend(gauge(
        exposure,
        cap,
        20,
        if exposure >= cap {
            Tone::Bad
        } else {
            Tone::Good
        },
    ));
    gauge_spans.extend([
        sep(),
        label("realized"),
        signed_usd(realized),
        muted(format!(" over {} closed", portfolio.closed_positions.len())),
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                label("bankroll"),
                plain(usd(decimal_f64(portfolio.bankroll))),
                muted(format!(
                    " (base {})",
                    usd(decimal_f64(app.settings.bankroll))
                )),
                sep(),
                label("position risk"),
                plain(format!(
                    "1-{}% · quarter Kelly",
                    decimal_f64(app.settings.maximum_position_fraction) * 100.0
                )),
                sep(),
                label("headroom"),
                plain(usd((cap - exposure).max(0.0))),
            ]),
            Line::from(gauge_spans),
        ]),
        inner,
    );

    render_open_positions(frame, open_area, app);
    if closed_height > 0 {
        render_closed_positions(frame, closed_area, app);
    }
}

fn render_open_positions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let portfolio = &app.store.portfolio;
    let inner = section(
        frame,
        area,
        "OPEN POSITIONS",
        vec![muted(format!("{}", portfolio.open_positions.len()))],
    );
    if portfolio.open_positions.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(muted("no paper positions")),
                Line::from(muted(
                    "open one from Markets with `o` on an actionable, news-reviewed row",
                )),
            ]),
            inner,
        );
        return;
    }
    let rows = portfolio
        .open_positions
        .iter()
        .enumerate()
        .map(|(index, position)| {
            let current = app
                .store
                .rows
                .iter()
                .find(|row| row.opportunity.id == position.opportunity_id);
            let (participant, status) = match current {
                Some(row) => (
                    row.opportunity.participant.clone(),
                    badge(
                        class_label(row.effective_class),
                        Tone::for_class(row.effective_class),
                    ),
                ),
                None => (position_name(position), muted("not in latest scan")),
            };
            let mut row = Row::new(vec![
                Cell::from(clock(position.opened_at)),
                Cell::from(fit(&participant, 26)),
                Cell::from(format!("{:?}", position.side).to_ascii_lowercase()),
                Cell::from(right(format!("{}", position.quantity.round_dp(2)))),
                Cell::from(right(price(decimal_f64(position.entry_price)))),
                Cell::from(right(usd(decimal_f64(position.maximum_loss)))),
                Cell::from(closing_cell(position)),
                Cell::from(clv_cell(position)),
                Cell::from(status),
                Cell::from(muted(position.opportunity_id.to_string())),
            ]);
            if index == app.portfolio_cursor {
                row = row.style(selected_row_style());
            }
            row
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(26),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(36),
        ],
    )
    .header(
        Row::new(vec![
            "opened",
            "participant",
            "side",
            "qty",
            "entry",
            "max loss",
            "closing",
            "clv",
            "now",
            "opportunity",
        ])
        .style(Tone::Muted.style()),
    )
    .column_spacing(1);
    frame.render_widget(table, inner);
}

/// Settled and manually closed positions, newest first.
fn render_closed_positions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let portfolio = &app.store.portfolio;
    let inner = section(
        frame,
        area,
        "CLOSED POSITIONS",
        vec![
            muted(format!("{}", portfolio.closed_positions.len())),
            sep(),
            label("realized"),
            signed_usd(decimal_f64(portfolio.realized_pnl)),
        ],
    );
    let rows = portfolio.closed_positions.iter().rev().map(|position| {
        let result = match position.realized_pnl {
            Some(pnl) => Line::from(signed_usd(decimal_f64(pnl))).alignment(Alignment::Right),
            None => Line::from(muted("closed by hand")).alignment(Alignment::Right),
        };
        let payout = position
            .settlement_payout
            .map(|value| price(decimal_f64(value)))
            .unwrap_or_else(|| "-".into());
        Row::new(vec![
            Cell::from(clock(position.closed_at.unwrap_or(position.opened_at))),
            Cell::from(fit(&position_name(position), 26)),
            Cell::from(format!("{:?}", position.side).to_ascii_lowercase()),
            Cell::from(right(format!("{}", position.quantity.round_dp(2)))),
            Cell::from(right(price(decimal_f64(position.entry_price)))),
            Cell::from(closing_cell(position)),
            Cell::from(clv_cell(position)),
            Cell::from(right(payout)),
            Cell::from(result),
            Cell::from(muted(position.opportunity_id.to_string())),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(26),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Min(36),
        ],
    )
    .header(
        Row::new(vec![
            "closed",
            "market",
            "side",
            "qty",
            "entry",
            "closing",
            "clv",
            "payout",
            "p&l",
            "opportunity",
        ])
        .style(Tone::Muted.style()),
    )
    .column_spacing(1);
    frame.render_widget(table, inner);
}

/// Market slug when the position recorded one, else the market id.
fn position_name(position: &PaperPosition) -> String {
    if position.market_slug.is_empty() {
        position.market_id.clone()
    } else {
        position.market_slug.clone()
    }
}

fn closing_cell(position: &PaperPosition) -> Line<'static> {
    match position.closing_price {
        Some(closing) => right(price(decimal_f64(closing))),
        None => Line::from(muted("-")).alignment(Alignment::Right),
    }
}

/// Closing-line value: positive when the entry beat the venue's pre-start price.
fn clv_cell(position: &PaperPosition) -> Line<'static> {
    match position.closing_line_value() {
        Some(clv) => Line::from(signed_pp(decimal_f64(clv))).alignment(Alignment::Right),
        None => Line::from(muted("-")).alignment(Alignment::Right),
    }
}

// ---------------------------------------------------------------- system

fn render_system(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [settings_area, log_area] =
        Layout::vertical([Constraint::Length(9), Constraint::Min(4)]).areas(area);
    let inner = section(frame, settings_area, "SETTINGS IN FORCE", Vec::new());
    let s = &app.settings;
    let lines = vec![
        Line::from(vec![
            label("edge gates"),
            plain(format!(
                "raw ≥ {} · net ≥ {} after dispersion and fees",
                s.minimum_raw_edge, s.minimum_net_edge
            )),
        ]),
        Line::from(vec![
            label("price band"),
            plain(format!("{} – {}", s.minimum_price, s.maximum_price)),
            sep(),
            label("quorum"),
            plain(format!(
                "{} families watchlist · {}{} actionable",
                s.watchlist_source_families,
                s.minimum_source_families,
                if s.require_reference_book {
                    " + reference"
                } else {
                    ""
                }
            )),
        ]),
        Line::from(vec![
            label("risk"),
            plain(format!(
                "bankroll {} · position {}–{}% · exposure cap {} · Kelly ×{}",
                s.bankroll,
                decimal_f64(s.minimum_position_fraction) * 100.0,
                decimal_f64(s.maximum_position_fraction) * 100.0,
                s.maximum_total_exposure,
                s.kelly_fraction
            )),
        ]),
        Line::from(vec![
            label("freshness"),
            plain(format!(
                "quotes {}s · confirmation {}s · book {}s",
                s.max_quote_age.as_secs(),
                s.confirmation_max_age.as_secs(),
                s.book_max_age.as_secs()
            )),
        ]),
        Line::from(vec![
            label("cadence"),
            plain(format!(
                "scan {} · discovery cache {} · retention {}d",
                duration_label(s.scan_interval.as_secs() as i64),
                duration_label(s.discovery_refresh.as_secs() as i64),
                s.retention_days
            )),
            sep(),
            label("concurrency"),
            plain(format!(
                "books {} · sources {}",
                s.book_concurrency, s.source_concurrency
            )),
        ]),
        Line::from(vec![
            label("polymarket"),
            plain(s.polymarket_base_url.clone()),
            sep(),
            label("sources"),
            plain(
                app.engine
                    .as_ref()
                    .map(|engine| engine.source_ids.join(", "))
                    .unwrap_or_else(|| "attached; see Sources view".into()),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    render_log(frame, log_area, app, usize::MAX, app.log_scroll);
}

/// Newest lines at the bottom; `scroll` counts lines hidden below the tail.
fn render_log(frame: &mut Frame<'_>, area: Rect, app: &App, limit: usize, scroll: usize) {
    let Some(logs) = &app.logs else {
        let inner = section(frame, area, "LOG", Vec::new());
        frame.render_widget(
            Paragraph::new(muted(
                "attached to the store; logs stay with the running process",
            )),
            inner,
        );
        return;
    };
    let lines = logs.snapshot();
    let detail = if scroll > 0 {
        vec![Span::styled(
            format!("▼ {scroll} newer"),
            Tone::Warn.style(),
        )]
    } else {
        vec![muted(format!("{} lines", lines.len()))]
    };
    let inner = section(frame, area, "LOG", detail);
    let height = usize::from(inner.height).min(limit).max(1);
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let width = usize::from(inner.width);
    let rendered = lines[start..end]
        .iter()
        .map(|line| {
            let tone = match line.level {
                tracing::Level::ERROR => Tone::Bad,
                tracing::Level::WARN => Tone::Warn,
                tracing::Level::INFO => Tone::Neutral,
                _ => Tone::Muted,
            };
            Line::from(vec![
                muted(format!("{} ", clock(line.at))),
                Span::styled(format!("{:<5} ", line.level), tone.bold()),
                muted(format!("{:<10} ", fit(&line.target, 10))),
                Span::styled(fit(&line.message, width.saturating_sub(27)), tone.style()),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rendered), inner);
}

// ---------------------------------------------------------------- helpers

fn class_label(class: RecommendationClass) -> &'static str {
    match class {
        RecommendationClass::Actionable => "ACTIONABLE",
        RecommendationClass::Watchlist => "WATCHLIST",
        RecommendationClass::Rejected => "rejected",
    }
}

fn sport_label(sport: Sport) -> String {
    sport.to_string().to_ascii_uppercase()
}

fn right(text: String) -> Line<'static> {
    Line::from(text).alignment(Alignment::Right)
}
