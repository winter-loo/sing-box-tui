use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const UI_STALL_THRESHOLD: Duration = Duration::from_secs(3);
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UiStallSnapshot {
    pub(crate) operation: String,
    pub(crate) stalled_for: Duration,
}

pub(crate) struct UiHeartbeat {
    state: Mutex<UiHeartbeatState>,
}

struct UiHeartbeatState {
    last_progress: Instant,
    operation: String,
    generation: u64,
}

impl UiHeartbeat {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            state: Mutex::new(UiHeartbeatState {
                last_progress: now,
                operation: "startup".to_string(),
                generation: 0,
            }),
        }
    }

    pub(crate) fn record_progress(&self, now: Instant, operation: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.last_progress = now;
        state.operation = operation.into();
        state.generation = state.generation.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn stalled_snapshot_at(
        &self,
        now: Instant,
        threshold: Duration,
    ) -> Option<UiStallSnapshot> {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let stalled_for = now.saturating_duration_since(state.last_progress);
        (stalled_for >= threshold).then(|| UiStallSnapshot {
            operation: state.operation.clone(),
            stalled_for,
        })
    }

    fn stalled_report_at(
        &self,
        now: Instant,
        threshold: Duration,
    ) -> Option<(u64, UiStallSnapshot)> {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let stalled_for = now.saturating_duration_since(state.last_progress);
        (stalled_for >= threshold).then(|| {
            (
                state.generation,
                UiStallSnapshot {
                    operation: state.operation.clone(),
                    stalled_for,
                },
            )
        })
    }
}

pub(crate) struct UiWatchdog {
    heartbeat: Arc<UiHeartbeat>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl UiWatchdog {
    pub(crate) fn start(log_path: PathBuf) -> Self {
        let heartbeat = Arc::new(UiHeartbeat::new(Instant::now()));
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_heartbeat = Arc::clone(&heartbeat);
        let worker_stopping = Arc::clone(&stopping);
        let worker = thread::spawn(move || {
            let mut reported_generation = None;
            while !worker_stopping.load(Ordering::Relaxed) {
                thread::sleep(WATCHDOG_POLL_INTERVAL);
                let Some((generation, snapshot)) = worker_heartbeat
                    .stalled_report_at(Instant::now(), UI_STALL_THRESHOLD)
                else {
                    continue;
                };
                if reported_generation == Some(generation) {
                    continue;
                }
                reported_generation = Some(generation);
                append_stall_diagnostic(&log_path, &snapshot);
                request_windows_process_dump(&log_path);
            }
        });
        Self {
            heartbeat,
            stopping,
            worker: Some(worker),
        }
    }

    pub(crate) fn progress(&self, operation: impl Into<String>) {
        self.heartbeat.record_progress(Instant::now(), operation);
    }
}

impl Drop for UiWatchdog {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn ui_watchdog_log_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("sing-box-tui-ui-watchdog.log")
}

fn append_stall_diagnostic(path: &Path, snapshot: &UiStallSnapshot) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!(
        "{timestamp} ui_stall pid={} stalled_ms={} operation={}\n",
        std::process::id(),
        snapshot.stalled_for.as_millis(),
        snapshot.operation
    );
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

#[cfg(windows)]
fn request_windows_process_dump(log_path: &Path) {
    let Some(executable) = find_procdump() else {
        append_watchdog_note(
            log_path,
            "procdump.exe is not on PATH or in the sing-box-tui user tools directory",
        );
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let dump_path = log_path.with_file_name(format!("sing-box-tui-hang-{timestamp}.dmp"));
    match Command::new(executable)
        .args(["-accepteula", "-ma", &std::process::id().to_string()])
        .arg(&dump_path)
        .spawn()
    {
        Ok(_) => append_watchdog_note(
            log_path,
            &format!("requested process dump at {}", dump_path.display()),
        ),
        Err(error) => append_watchdog_note(
            log_path,
            &format!("failed to launch procdump: {error}"),
        ),
    }
}

#[cfg(windows)]
fn find_procdump() -> Option<PathBuf> {
    if let Ok(output) = Command::new("where.exe").arg("procdump.exe").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(path) = output
            .status
            .success()
            .then(|| stdout.lines().next().unwrap_or_default().trim())
            .filter(|path| !path.is_empty())
        {
            return Some(PathBuf::from(path));
        }
    }

    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|base| base.join("sing-box-tui/tools/procdump/procdump.exe"))
        .filter(|path| path.is_file())
}

#[cfg(not(windows))]
fn request_windows_process_dump(_log_path: &Path) {}

fn append_watchdog_note(path: &Path, message: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "watchdog_note {message}");
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_ui_heartbeat_reports_the_last_operation_once_threshold_is_crossed() {
        let started = Instant::now();
        let heartbeat = UiHeartbeat::new(started);
        heartbeat.record_progress(started, "poll_private_access_updates");

        assert_eq!(
            heartbeat.stalled_snapshot_at(
                started + Duration::from_secs(4),
                Duration::from_secs(3),
            ),
            Some(UiStallSnapshot {
                operation: "poll_private_access_updates".to_string(),
                stalled_for: Duration::from_secs(4),
            })
        );
    }
}
