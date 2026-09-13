//! Liveness for unattended operation. The `local` engine publishes a
//! `health.json` snapshot on every status change (atomic rename, so a reader
//! never sees a partial file) and, under systemd, sends `READY=1` once the
//! loops are up and `WATCHDOG=1` after every scan attempt. A hung scan stops
//! the pings and `WatchdogSec` restarts the unit; a loop that is alive but
//! failing keeps pinging and is visible in the snapshot instead, because a
//! restart would not fix an unreachable source.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Result,
    storage::write_json_atomic,
    tui::feed::{EngineStatus, Phase},
};

/// Short git commit the binary was built from; `unknown` without git.
pub const GIT_COMMIT: &str = match option_env!("POLYBOT_GIT_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

pub const HEALTH_FILE: &str = "health.json";

/// What an operator or watchdog needs to judge the engine from outside the
/// process. Every field is also in the TUI's status panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub captured_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub pid: u32,
    pub git_commit: String,
    pub phase: String,
    pub scans_completed: u64,
    pub scans_failed: u64,
    pub last_scan_id: Option<Uuid>,
    pub last_scan_completed_at: Option<DateTime<Utc>>,
    pub last_scan_duration_ms: Option<u64>,
    pub last_candidate_count: Option<usize>,
    pub next_scan_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub reviewer: Option<String>,
    pub last_news_pass_at: Option<DateTime<Utc>>,
    pub sources: Vec<String>,
    /// Whether scans are also archived in Postgres.
    pub history_archive: bool,
}

impl HealthSnapshot {
    pub fn from_status(status: &EngineStatus, history_archive: bool, now: DateTime<Utc>) -> Self {
        Self {
            captured_at: now,
            started_at: status.started_at,
            pid: std::process::id(),
            git_commit: GIT_COMMIT.to_string(),
            phase: phase_text(&status.phase).to_string(),
            scans_completed: status.scans_completed,
            scans_failed: status.scans_failed,
            last_scan_id: status.last_scan.as_ref().map(|scan| scan.scan_id),
            last_scan_completed_at: status.last_scan.as_ref().map(|scan| scan.completed_at),
            last_scan_duration_ms: status.last_scan_duration_ms,
            last_candidate_count: status.last_scan.as_ref().map(|scan| scan.candidate_count),
            next_scan_at: status.next_scan_at,
            last_error: status.last_error.clone(),
            reviewer: status.reviewer.map(str::to_string),
            last_news_pass_at: status.last_news_pass.map(|(at, _)| at),
            sources: status.source_ids.clone(),
            history_archive,
        }
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }

    /// One line for `systemctl status`.
    pub fn status_line(&self) -> String {
        format!(
            "{} | scans ok={} failed={} | last error: {}",
            self.phase,
            self.scans_completed,
            self.scans_failed,
            self.last_error.as_deref().unwrap_or("none")
        )
    }
}

fn phase_text(phase: &Phase) -> &'static str {
    match phase {
        Phase::Starting => "starting",
        Phase::Idle => "idle",
        Phase::Scanning { .. } => "scanning",
        Phase::Reviewing => "reviewing",
        Phase::ShuttingDown => "shutting_down",
    }
}

/// systemd notifications. Each is a no-op when `NOTIFY_SOCKET` is unset
/// (interactive runs, other init systems, non-Linux hosts), and a failure to
/// notify is never fatal: the unit's watchdog would restart us anyway.
pub mod systemd {
    #[cfg(target_os = "linux")]
    use sd_notify::NotifyState;

    /// The loops are up; systemd may consider the unit started.
    pub fn ready() {
        #[cfg(target_os = "linux")]
        let _ = sd_notify::notify(&[NotifyState::Ready]);
    }

    /// A scan attempt finished (success or failure): the loop is alive.
    pub fn watchdog() {
        #[cfg(target_os = "linux")]
        let _ = sd_notify::notify(&[NotifyState::Watchdog]);
    }

    pub fn status(line: &str) {
        #[cfg(target_os = "linux")]
        let _ = sd_notify::notify(&[NotifyState::Status(line)]);
        #[cfg(not(target_os = "linux"))]
        let _ = line;
    }

    pub fn stopping() {
        #[cfg(target_os = "linux")]
        let _ = sd_notify::notify(&[NotifyState::Stopping]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_and_summarizes() {
        let mut status = EngineStatus::new(vec!["pinnacle".into()], Some("keyword"));
        status.scans_completed = 3;
        status.last_error = Some("boom".into());
        let snapshot = HealthSnapshot::from_status(&status, true, Utc::now());
        assert_eq!(snapshot.phase, "starting");
        assert_eq!(snapshot.sources, vec!["pinnacle"]);
        assert!(snapshot.history_archive);
        assert_eq!(
            snapshot.status_line(),
            "starting | scans ok=3 failed=0 | last error: boom"
        );
        let dir = std::env::temp_dir().join(format!("polybot-health-{}", Uuid::new_v4()));
        let path = dir.join(HEALTH_FILE);
        snapshot.write(&path).unwrap();
        let read: HealthSnapshot = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.scans_completed, 3);
        assert_eq!(read.git_commit, GIT_COMMIT);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
