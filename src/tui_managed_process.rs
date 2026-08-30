use std::path::Path;

use anyhow::{Context, Result};

use crate::benchmark_workflow::{BenchmarkWorkflow, ManagedRuntimeObservation};
use crate::controller::ApiClient;
use crate::managed_sing_box::{LifecycleReport, ManagedSingBox};

use super::App;

pub(super) fn restart_managed_sing_box_with_quality_confirmation(
    benchmark_workflow: &mut BenchmarkWorkflow,
    sing_box: &mut ManagedSingBox,
    client: &ApiClient,
    config_path: &Path,
    database_path: &Path,
) -> Result<LifecycleReport> {
    let report =
        benchmark_workflow.confirm_managed_runtime_reload(config_path, database_path, || {
            let receipt = sing_box.restart()?;
            // Every feature-triggered restart must prove controller readiness while the config and
            // quality generation remain locked; process creation alone cannot release the fence.
            receipt.observe_controller(client)?;
            let report = receipt.into_report();
            let pid = report.started_pid();
            Ok(ManagedRuntimeObservation::new(
                report,
                config_path,
                &client.base_url,
                Some(pid),
            ))
        })?;
    let runtime_receipt = benchmark_workflow
        .runtime_receipt()
        .cloned()
        .context("managed restart did not produce a runtime receipt")?;
    if let Err(error) = sing_box.register_confirmed_active_environment(&runtime_receipt) {
        benchmark_workflow.pause_quality_persistence();
        return Err(error).context("failed to publish confirmed active runtime environment");
    }
    Ok(report)
}

impl App {
    pub(super) fn network_transition_is_running(&self) -> bool {
        self.system_proxy.is_updating() || self.internet_tun.is_transitioning()
    }

    pub(super) fn shutdown_runtime_environment(&mut self) -> Result<()> {
        // A declared program may own isolated node-runtime-manager descendants. Stop it before
        // deciding whether the live sing-box stays up; the custom probe never inherits the TUI's
        // background ownership permission for the user's main proxy runtime.
        let mut errors = Vec::new();
        if let Err(error) = self.cancel_active_usability_probe() {
            errors.push(format!(
                "failed to finalize cancelled usability probe: {error:#}"
            ));
        }
        if self.sing_box.is_leaving_running() {
            if errors.is_empty() {
                return Ok(());
            }
            anyhow::bail!(errors.join("; "));
        }

        if let Err(error) = self.save_runtime_state() {
            errors.push(format!("failed to preserve runtime intent: {error:#}"));
        }
        let proxy_suspended = match self.system_proxy.suspend_for_exit() {
            Ok(_) => true,
            Err(error) => {
                errors.push(format!("failed to disable system proxy: {error:#}"));
                false
            }
        };
        if let Err(error) = self.internet_tun.suspend_for_exit() {
            errors.push(format!("failed to suspend Internet TUN: {error:#}"));
        }
        if proxy_suspended {
            if let Err(error) = self.shutdown_managed_sing_box() {
                errors.push(format!("failed to stop managed sing-box: {error:#}"));
            }
        } else {
            self.sing_box.leave_running();
            errors.push(
                "kept managed sing-box running because the system proxy is still enabled"
                    .to_string(),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(errors.join("; "))
        }
    }

    pub(super) fn start_managed_sing_box(&mut self) -> Result<()> {
        let config_path = &self.system_proxy_config_path;
        let database_path = &self.node_quality_db_path;
        let client = &self.client;
        let sing_box = &mut self.sing_box;
        let report = self.benchmark_workflow.confirm_managed_runtime_reload(
            config_path,
            database_path,
            || {
                let report = sing_box.start(client)?;
                let pid = report.started_pid();
                Ok(ManagedRuntimeObservation::new(
                    report,
                    config_path,
                    &client.base_url,
                    Some(pid),
                ))
            },
        )?;
        let runtime_receipt = self
            .benchmark_workflow
            .runtime_receipt()
            .cloned()
            .context("managed startup did not produce a runtime receipt")?;
        if let Err(error) = self
            .sing_box
            .register_confirmed_active_environment(&runtime_receipt)
        {
            self.benchmark_workflow.pause_quality_persistence();
            return Err(error).context("failed to publish confirmed active runtime environment");
        }
        if report.replaced_existing() {
            self.status = format!("Restarted managed sing-box {}", report.transition());
        } else {
            self.status = format!("Started managed sing-box {}", report.started_process());
        }
        Ok(())
    }

    pub(super) fn restart_managed_sing_box(&mut self) -> Result<()> {
        restart_managed_sing_box_with_quality_confirmation(
            &mut self.benchmark_workflow,
            &mut self.sing_box,
            &self.client,
            &self.system_proxy_config_path,
            &self.node_quality_db_path,
        )?;
        Ok(())
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
    use serde_json::json;

    use super::super::test_support::{test_app, test_state_path};
    use crate::config::{RouteAutoDetectInterfaceState, default_tun_inbound, inspect_tun_config};
    use crate::internet_tun::{InternetTunTransaction, PersistedInternetTun};
    use crate::system_proxy::SystemProxy;

    #[test]
    fn uppercase_b_keeps_managed_sing_box_running_and_exits_tui() {
        let mut app = test_app();
        let keep_running = app.handle_key(KeyCode::Char('B')).unwrap();
        assert!(!keep_running);
        assert!(app.sing_box.is_leaving_running());
    }

    #[test]
    fn ordinary_exit_suspends_tun_without_erasing_restore_intent() {
        let path = test_state_path();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "inbounds": [default_tun_inbound()],
                "route": { "auto_detect_interface": true }
            }))
            .unwrap(),
        )
        .unwrap();
        let persisted =
            PersistedInternetTun::new(Some(true), Some(RouteAutoDetectInterfaceState::Disabled));
        let mut app = test_app();
        app.internet_tun = InternetTunTransaction::new(path.clone(), persisted).unwrap();

        app.shutdown_runtime_environment().unwrap();

        assert!(!inspect_tun_config(&path).unwrap().managed_internet_tun);
        assert_eq!(app.internet_tun.persisted(), persisted);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn background_exit_keeps_tun_configuration_active() {
        let path = test_state_path();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "inbounds": [default_tun_inbound()],
                "route": { "auto_detect_interface": true }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut app = test_app();
        app.internet_tun =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::new(Some(true), None))
                .unwrap();
        app.sing_box.leave_running();

        app.shutdown_runtime_environment().unwrap();

        assert!(inspect_tun_config(&path).unwrap().managed_internet_tun);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn proxy_cleanup_failure_keeps_managed_sing_box_alive() {
        let mut app = test_app();
        app.system_proxy = SystemProxy::failing_for_test(test_state_path(), "127.0.0.1:6780", true);

        let error = app
            .shutdown_runtime_environment()
            .expect_err("proxy cleanup failure is reported");

        assert!(error.to_string().contains("failed to disable system proxy"));
        assert!(app.sing_box.is_leaving_running());
    }
}
