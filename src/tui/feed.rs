//! Live status published by an in-process engine (the `local` supervisor) and
//! commands flowing back from the UI. An attached `tui` has no engine and
//! renders from the store alone.

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, watch};

use crate::domain::ScanSummary;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Idle,
    Scanning { since: DateTime<Utc> },
    Reviewing,
    ShuttingDown,
}

#[derive(Debug, Clone)]
pub struct EngineStatus {
    pub started_at: DateTime<Utc>,
    pub phase: Phase,
    pub scans_completed: u64,
    pub scans_failed: u64,
    pub last_scan: Option<ScanSummary>,
    pub last_scan_duration_ms: Option<u64>,
    pub next_scan_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub reviewer: Option<&'static str>,
    pub last_news_pass: Option<(DateTime<Utc>, usize)>,
    pub source_ids: Vec<String>,
}

impl EngineStatus {
    pub fn new(source_ids: Vec<String>, reviewer: Option<&'static str>) -> Self {
        Self {
            started_at: Utc::now(),
            phase: Phase::Starting,
            scans_completed: 0,
            scans_failed: 0,
            last_scan: None,
            last_scan_duration_ms: None,
            next_scan_at: None,
            last_error: None,
            reviewer,
            last_news_pass: None,
            source_ids,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommand {
    ScanNow,
    Shutdown,
}

/// UI-side handle: observe status, send commands.
#[derive(Clone)]
pub struct EngineHandle {
    pub status: watch::Receiver<EngineStatus>,
    pub commands: mpsc::UnboundedSender<EngineCommand>,
}

/// Engine-side handle: publish status, receive commands.
pub struct EnginePort {
    pub status: watch::Sender<EngineStatus>,
    pub commands: mpsc::UnboundedReceiver<EngineCommand>,
}

pub fn engine_channel(initial: EngineStatus) -> (EnginePort, EngineHandle) {
    let (status_tx, status_rx) = watch::channel(initial);
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    (
        EnginePort {
            status: status_tx,
            commands: command_rx,
        },
        EngineHandle {
            status: status_rx,
            commands: command_tx,
        },
    )
}
