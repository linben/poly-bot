//! Presentation primitives: one palette, badges, number formatting, text
//! fitting, key hints, section frames. No knowledge of scans or markets.

use chrono::{DateTime, Utc};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Muted,
    Good,
    Warn,
    Bad,
    Accent,
}

impl Tone {
    pub fn style(self) -> Style {
        match self {
            Tone::Neutral => Style::default(),
            Tone::Muted => Style::default().fg(Color::DarkGray),
            Tone::Good => Style::default().fg(Color::Green),
            Tone::Warn => Style::default().fg(Color::Yellow),
            Tone::Bad => Style::default().fg(Color::Red),
            Tone::Accent => Style::default().fg(Color::Cyan),
        }
    }

    pub fn bold(self) -> Style {
        self.style().add_modifier(Modifier::BOLD)
    }

    pub fn for_class(class: crate::domain::RecommendationClass) -> Tone {
        match class {
            crate::domain::RecommendationClass::Actionable => Tone::Good,
            crate::domain::RecommendationClass::Watchlist => Tone::Warn,
            crate::domain::RecommendationClass::Rejected => Tone::Muted,
        }
    }

    pub fn for_effect(effect: &str) -> Tone {
        match effect {
            "unchanged" => Tone::Good,
            "lower" | "review" => Tone::Warn,
            "reject" => Tone::Bad,
            _ => Tone::Neutral,
        }
    }

    pub fn for_signed(value: f64) -> Tone {
        if value > 0.0 {
            Tone::Good
        } else if value < 0.0 {
            Tone::Bad
        } else {
            Tone::Neutral
        }
    }
}

pub fn badge(text: impl Into<String>, tone: Tone) -> Span<'static> {
    Span::styled(text.into(), tone.bold())
}

pub fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Tone::Muted.style())
}

pub fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text} "), Tone::Muted.style())
}

pub fn sep() -> Span<'static> {
    Span::styled(" · ", Tone::Muted.style())
}

pub fn plain(text: impl Into<String>) -> Span<'static> {
    Span::raw(text.into())
}

/// Probability or edge as percentage points with one decimal: `52.3%`.
pub fn pct(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

/// Signed percentage points: `+0.8pp`.
pub fn signed_pp(value: f64) -> Span<'static> {
    Span::styled(
        format!("{:+.2}pp", value * 100.0),
        Tone::for_signed(value).style(),
    )
}

pub fn price(value: f64) -> String {
    format!("{value:.3}")
}

pub fn usd(value: f64) -> String {
    format!("${value:.2}")
}

/// Age of an instant as `4s`, `2m05s`, `1h12m`, or `-` when unknown.
pub fn age(since: Option<DateTime<Utc>>) -> String {
    let Some(since) = since else {
        return "-".into();
    };
    let seconds = (Utc::now() - since).num_seconds();
    duration_label(seconds)
}

/// `in 42s` / `in 4m10s` / `due` for a future instant.
pub fn countdown(until: Option<DateTime<Utc>>) -> String {
    let Some(until) = until else {
        return "-".into();
    };
    let seconds = (until - Utc::now()).num_seconds();
    if seconds <= 0 {
        "due".into()
    } else {
        format!("in {}", duration_label(seconds))
    }
}

pub fn duration_label(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub fn millis_label(millis: u64) -> String {
    if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.2}s", millis as f64 / 1000.0)
    }
}

/// UTC clock `HH:MM:SS`.
pub fn clock(value: DateTime<Utc>) -> String {
    value.format("%H:%M:%S").to_string()
}

/// Truncate to `width` columns with an ellipsis.
pub fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn key_hints(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(hints.len() * 3 + 1);
    spans.push(Span::raw(" "));
    for (index, (key, action)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(sep());
        }
        spans.push(Span::styled((*key).to_string(), Tone::Accent.bold()));
        spans.push(Span::raw(format!(" {action}")));
    }
    Line::from(spans)
}

/// Top-rule section with a bold title; returns the inner area.
pub fn section(frame: &mut Frame<'_>, area: Rect, title: &str, detail: Vec<Span<'static>>) -> Rect {
    let mut spans = vec![Span::styled(format!(" {title} "), Tone::Neutral.bold())];
    if !detail.is_empty() {
        spans.push(muted(" "));
        spans.extend(detail);
        spans.push(Span::raw(" "));
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Tone::Muted.style())
        .title(Line::from(spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

pub fn selected_row_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Horizontal gauge of `value` in `[0, max]`, `width` cells.
pub fn gauge(value: f64, max: f64, width: usize, tone: Tone) -> Vec<Span<'static>> {
    let width = width.max(1);
    let max = if max.is_finite() && max > 0.0 {
        max
    } else {
        1.0
    };
    let filled = ((value / max) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    vec![
        muted("▕"),
        Span::styled("█".repeat(filled), tone.style()),
        muted("·".repeat(width - filled)),
        muted("▏"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_truncates_with_ellipsis() {
        assert_eq!(fit("Baltimore Ravens", 9), "Baltimor…");
        assert_eq!(fit("short", 9), "short");
        assert_eq!(fit("anything", 0), "");
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(duration_label(42), "42s");
        assert_eq!(duration_label(125), "2m05s");
        assert_eq!(duration_label(4320), "1h12m");
        assert_eq!(millis_label(443), "443ms");
        assert_eq!(millis_label(3283), "3.28s");
    }
}
