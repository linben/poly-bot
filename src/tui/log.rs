//! Tracing layer that keeps the newest log events in memory while the UI owns
//! the terminal. Writing to stderr would corrupt the alternate screen.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use tracing::{Level, Subscriber, field::Field};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: DateTime<Utc>,
    pub level: Level,
    pub target: String,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<LogLine>>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity: capacity.max(1),
        }
    }

    /// Install this buffer as the global tracing subscriber with `filter`.
    /// Returns `None` when a subscriber is already set (tests, embedding).
    pub fn install(filter: tracing_subscriber::EnvFilter, capacity: usize) -> Option<Self> {
        use tracing_subscriber::prelude::*;
        let buffer = Self::new(capacity);
        tracing_subscriber::registry()
            .with(filter)
            .with(buffer.clone())
            .try_init()
            .ok()?;
        Some(buffer)
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.lines
            .lock()
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.lines.lock().map(|lines| lines.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&self, line: LogLine) {
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() == self.capacity {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }
}

struct MessageVisitor {
    message: String,
    fields: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

impl<S> Layer<S> for LogBuffer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut visitor = MessageVisitor {
            message: String::new(),
            fields: String::new(),
        };
        event.record(&mut visitor);
        let metadata = event.metadata();
        let target = metadata
            .target()
            .rsplit("::")
            .next()
            .unwrap_or(metadata.target())
            .to_string();
        self.push(LogLine {
            at: Utc::now(),
            level: *metadata.level(),
            target,
            message: format!("{}{}", visitor.message, visitor.fields),
        });
    }
}
