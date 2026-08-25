use anyhow::Result;

use super::App;
use crate::managed_sing_box::RestartReceipt;

impl App {
    pub(super) fn start_managed_sing_box(&mut self) -> Result<()> {
        let report = self.sing_box.start(&self.client)?;
        if report.replaced_existing() {
            self.status = format!("Restarted managed sing-box {}", report.transition());
        } else {
            self.status = format!("Started managed sing-box {}", report.started_process());
        }
        Ok(())
    }

    pub(super) fn restart_managed_sing_box(&mut self) -> Result<RestartReceipt> {
        self.sing_box.restart()
    }

    pub(super) fn shutdown_managed_sing_box(&mut self) -> Result<()> {
        if self.sing_box.is_leaving_running() {
            return Ok(());
        }
        if self.background_worker_management_enabled() {
            self.stop_live_background_auto_pick_task()?;
        }
        self.sing_box.shutdown()
    }

    pub(super) fn keep_sing_box_running_in_background(&mut self) -> Result<bool> {
        let auto_pick_pid = if self.auto_select_enabled {
            match self.ensure_auto_pick_background_worker() {
                Ok(worker) => Some(worker.pid()),
                Err(error) => {
                    self.set_status_only(format!(
                        "Failed to start background auto-pick: {error:#}"
                    ));
                    return Ok(true);
                }
            }
        } else {
            None
        };
        let private_access_sessions = match self.detach_private_access_for_background() {
            Ok(count) => count,
            Err(error) => {
                self.set_status_only(format!(
                    "Failed to keep Private Access running in background: {error:#}"
                ));
                return Ok(true);
            }
        };
        if let Err(error) = self.save_runtime_state() {
            self.set_status_only(format!(
                "Failed to save background Private Access state: {error:#}"
            ));
            return Ok(true);
        }
        self.sing_box.leave_running();
        let mut parts = vec!["sing-box".to_string()];
        if let Some(pid) = auto_pick_pid {
            parts.push(format!("auto-pick pid {pid}"));
        }
        if private_access_sessions > 0 {
            parts.push(format!(
                "{private_access_sessions} Private Access session(s)"
            ));
        }
        self.set_status_only(format!(
            "Leaving TUI; {} continue in background",
            parts.join(", ")
        ));
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::super::test_support::test_app;

    #[test]
    fn uppercase_b_keeps_managed_sing_box_running_and_exits_tui() {
        let mut app = test_app();
        let keep_running = app.handle_key(KeyCode::Char('B')).unwrap();
        assert!(!keep_running);
        assert!(app.sing_box.is_leaving_running());
    }
}
