use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::view::{format_duration_badge, subscription_report_badge, truncate_for_width};
use super::{App, SUBSCRIPTION_REFRESH_RETRY_INTERVAL, TuiSubscriptionRefreshOptions};
use crate::managed_sing_box::AuthorizationRequirement;
use crate::subscriptions::{
    SubscriptionRefreshOutput, SubscriptionRefreshRequest, refresh_subscriptions,
    validate_subscription_refresh_paths,
};

pub(super) struct SubscriptionRefreshState {
    request: SubscriptionRefreshRequest,
    interval: Duration,
    next_run: Instant,
    job: Option<SubscriptionRefreshJob>,
    last_report: Option<SubscriptionRefreshOutput>,
    last_error: Option<String>,
    pending_reload: bool,
    next_reload: Option<Instant>,
    last_reload_error: Option<String>,
}

struct SubscriptionRefreshJob {
    receiver: mpsc::Receiver<SubscriptionRefreshEvent>,
    worker: JoinHandle<()>,
}

enum SubscriptionRefreshEvent {
    Finished(Result<SubscriptionRefreshOutput, String>),
}

impl SubscriptionRefreshState {
    pub(super) fn from_options(
        options: TuiSubscriptionRefreshOptions,
        node_quality_db_path: PathBuf,
    ) -> Result<Option<Self>> {
        if options.disabled || !options.input.exists() {
            return Ok(None);
        }
        if options.interval_days == 0 {
            bail!("--subscription-interval-days must be greater than 0");
        }
        let interval = Duration::from_secs(options.interval_days.saturating_mul(24 * 60 * 60));
        let request = SubscriptionRefreshRequest {
            input: options.input,
            cache_path: options.cache_path,
            config_path: options.config_path.clone(),
            merged_path: options.config_path,
            node_quality_db_path,
            replace_nodes: false,
            include_geosite_rules: options.include_geosite_rules,
            include_tun_mode: options.include_tun_mode,
            force: options.force,
            interval_days: options.interval_days,
        };
        validate_subscription_refresh_paths(&request)?;
        Ok(Some(Self {
            request,
            interval,
            next_run: Instant::now(),
            job: None,
            last_report: None,
            last_error: None,
            pending_reload: false,
            next_reload: None,
            last_reload_error: None,
        }))
    }

    fn schedule_after(&mut self, delay: Duration) {
        self.next_run = Instant::now()
            .checked_add(delay)
            .unwrap_or_else(Instant::now);
    }
}

impl App {
    pub(super) fn subscription_summary_line(&self) -> String {
        let Some(state) = &self.subscription_refresh else {
            return "subscriptions: disabled or no .suburl".to_string();
        };
        if state.job.is_some() {
            return format!(
                "subscriptions: refreshing {} -> {}",
                state.request.input.display(),
                state.request.merged_path.display()
            );
        }
        if state.pending_reload {
            let retry = state
                .next_reload
                .map(|at| {
                    format!(
                        "  retry in {}",
                        format_duration_badge(at.saturating_duration_since(Instant::now()))
                    )
                })
                .unwrap_or_default();
            let error = state
                .last_reload_error
                .as_deref()
                .map(|error| format!("{}", truncate_for_width(error, 72)))
                .unwrap_or_else(|| "applying updated config".to_string());
            return format!("subscriptions: sing-box reload pending: {error}{retry}");
        }
        if let Some(error) = &state.last_error {
            return format!(
                "subscriptions: error: {}  retry in {}",
                truncate_for_width(error, 72),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
            );
        }
        if let Some(report) = &state.last_report {
            return format!(
                "subscriptions: {}  next in {}",
                subscription_report_badge(report),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now())),
            );
        }
        format!(
            "subscriptions: pending first refresh from {}",
            state.request.input.display()
        )
    }

    pub(super) fn maybe_start_subscription_refresh(&mut self) {
        if self
            .subscription_refresh
            .as_ref()
            .is_some_and(|state| state.pending_reload)
        {
            self.try_pending_subscription_reload_with(
                |app| app.reload_subscription_managed_runtime(),
                |app| app.refresh(),
            );
            return;
        }
        let Some(state) = self.subscription_refresh.as_mut() else {
            return;
        };
        if state.job.is_some() || Instant::now() < state.next_run {
            return;
        }
        self.start_subscription_refresh_job(false, "Refreshing subscriptions in background...");
    }

    pub(super) fn start_manual_subscription_refresh(&mut self) {
        if self.subscription_refresh.is_none() {
            self.set_status_only("Subscription refresh is disabled or .suburl was not found");
            return;
        }
        let started = self.start_subscription_refresh_job(
            true,
            "Manually refreshing subscriptions in background...",
        );
        if !started {
            self.set_status_only("Subscription refresh is already running");
        }
    }

    fn start_subscription_refresh_job(&mut self, force: bool, status: &str) -> bool {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return false;
        };
        if state.job.is_some() {
            return false;
        }
        let mut request = state.request.clone();
        request.force = force || state.request.force;
        state.request.force = false;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = refresh_subscriptions(&request).map_err(|error| error.to_string());
            let _ = tx.send(SubscriptionRefreshEvent::Finished(result));
        });
        state.job = Some(SubscriptionRefreshJob {
            receiver: rx,
            worker,
        });
        state.last_error = None;
        self.set_status_only(status);
        true
    }

    pub(super) fn poll_subscription_refresh_updates(&mut self) -> Result<()> {
        self.poll_subscription_refresh_updates_with_reload(|app| {
            app.reload_subscription_managed_runtime()
        })
    }

    pub(super) fn subscription_reload_needs_terminal_prompt(&self) -> bool {
        self.subscription_refresh
            .as_ref()
            .is_some_and(|state| state.pending_reload)
            && self
                .subscription_reload_authorization_requirement()
                .is_ok_and(|requirement| requirement == AuthorizationRequirement::InteractiveSudo)
    }

    fn subscription_reload_authorization_requirement(&self) -> Result<AuthorizationRequirement> {
        let next_run_needs_elevation = self.sing_box.startup_authorization_requirement()?
            == AuthorizationRequirement::InteractiveSudo;
        Ok(self
            .sing_box
            .restart_authorization_requirement(next_run_needs_elevation))
    }

    fn reload_subscription_managed_runtime(&mut self) -> Result<()> {
        self.sing_box.validate_config_for_restart()?;
        if self.subscription_reload_authorization_requirement()?
            == AuthorizationRequirement::InteractiveSudo
        {
            let status = Command::new("sudo")
                .args(["-n", "-v"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to check sudo authorization before sing-box reload")?;
            ensure!(
                status.success(),
                "sudo authorization expired; press R in the node list to authorize and retry"
            );
        }
        self.restart_managed_sing_box()
    }

    pub(super) fn retry_subscription_reload_now(&mut self) {
        let Some(state) = self.subscription_refresh.as_mut() else {
            self.set_status_only("Subscription refresh is disabled");
            return;
        };
        if !state.pending_reload {
            self.set_status_only("No subscription config reload is pending");
            return;
        }
        state.next_reload = Some(Instant::now());
        self.try_pending_subscription_reload_with(
            |app| app.reload_subscription_managed_runtime(),
            |app| app.refresh(),
        );
    }

    fn poll_subscription_refresh_updates_with_reload<F>(&mut self, reload: F) -> Result<()>
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return Ok(());
        };
        let Some(job) = state.job.as_ref() else {
            return Ok(());
        };
        let event = match job.receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => SubscriptionRefreshEvent::Finished(Err(
                "subscription refresh worker disconnected".to_string(),
            )),
        };
        let job = state.job.take().expect("subscription refresh job exists");
        let _ = job.worker.join();
        match event {
            SubscriptionRefreshEvent::Finished(Ok(report)) => {
                if report.node_history_changed {
                    // The live controller still serves the pre-refresh config. Do not rebind its
                    // in-flight or subsequent probe facts to the new on-disk identity generation.
                    self.benchmark_workflow.pause_quality_persistence();
                    self.usability_probe_projection_cache.clear();
                    self.node_quality_detail = None;
                }
                state.schedule_after(state.interval);
                state.last_error = None;
                state.last_report = Some(report.clone());
                if report.config_updated || report.node_history_changed {
                    state.pending_reload = true;
                    state.next_reload = Some(Instant::now());
                    state.last_reload_error = None;
                    self.try_pending_subscription_reload_with(reload, |app| app.refresh());
                } else {
                    self.set_status_only(format!(
                        "Subscription refresh kept the active config unchanged: {}",
                        subscription_report_badge(&report)
                    ));
                }
            }
            SubscriptionRefreshEvent::Finished(Err(error)) => {
                state.schedule_after(SUBSCRIPTION_REFRESH_RETRY_INTERVAL);
                state.last_error = Some(error.clone());
                self.set_status_only(format!(
                    "Subscription refresh failed: {}; retry in {}",
                    truncate_for_width(&error, 80),
                    format_duration_badge(SUBSCRIPTION_REFRESH_RETRY_INTERVAL)
                ));
            }
        }
        Ok(())
    }

    fn try_pending_subscription_reload_with<F, Refresh>(&mut self, reload: F, refresh: Refresh)
    where
        F: FnOnce(&mut Self) -> Result<()>,
        Refresh: FnOnce(&mut Self) -> Result<()>,
    {
        let Some(state) = self.subscription_refresh.as_ref() else {
            return;
        };
        if !state.pending_reload
            || state.job.is_some()
            || state.next_reload.is_some_and(|at| Instant::now() < at)
            || self.network_transition_is_running()
        {
            return;
        }
        let result = reload(self);
        let state = self
            .subscription_refresh
            .as_mut()
            .expect("subscription refresh exists");
        let report = state.last_report.as_ref().map(subscription_report_badge);
        match result {
            Ok(()) => {
                state.pending_reload = false;
                state.next_reload = None;
                state.last_reload_error = None;
                let refresh_error = refresh(self).err();
                let status = format!(
                    "Subscription refresh applied and sing-box restarted: {}",
                    report.unwrap_or_else(|| "updated config".to_string())
                );
                if let Some(error) = refresh_error {
                    self.set_status_only(format!("{status}; node list refresh failed: {error:#}"));
                } else {
                    self.set_status_only(status);
                }
            }
            Err(error) => {
                let error = format!("{error:#}");
                state.last_reload_error = Some(error.clone());
                state.next_reload = Instant::now().checked_add(SUBSCRIPTION_REFRESH_RETRY_INTERVAL);
                self.set_status_only(format!(
                    "Subscription refresh needs sing-box reload; failed: {}; retry in {}",
                    truncate_for_width(&error, 90),
                    format_duration_badge(SUBSCRIPTION_REFRESH_RETRY_INTERVAL)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use std::sync::atomic::Ordering;

    use super::super::NodeQualityDetailState;
    use super::{SubscriptionRefreshEvent, SubscriptionRefreshJob, SubscriptionRefreshState};
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome};
    use crate::storage::{BenchmarkStore, StoredUsabilityProbeRun};
    use crate::subscriptions::{SubscriptionRefreshOutput, SubscriptionRefreshRequest};

    use super::super::test_support::{test_app, test_db_path};

    fn install_completed_refresh(
        app: &mut super::super::App,
        database_path: &std::path::Path,
        config_updated: bool,
        node_history_changed: bool,
    ) {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(SubscriptionRefreshEvent::Finished(Ok(
                SubscriptionRefreshOutput {
                    input_path: ".suburl".to_string(),
                    cache_path: ".suburl.cache.json".to_string(),
                    interval_days: 7,
                    merged_config_path: "config.json".to_string(),
                    backup_config_path: None,
                    config_updated,
                    node_history_reconciled: true,
                    node_history_changed,
                    node_quality_generation: Some(u64::from(node_history_changed)),
                    providers: Vec::new(),
                },
            )))
            .expect("queue completed subscription refresh");
        app.subscription_refresh = Some(SubscriptionRefreshState {
            request: SubscriptionRefreshRequest {
                input: ".suburl".into(),
                cache_path: ".suburl.cache.json".into(),
                config_path: "config.json".into(),
                merged_path: "config.json".into(),
                node_quality_db_path: database_path.to_path_buf(),
                replace_nodes: false,
                include_geosite_rules: false,
                include_tun_mode: false,
                force: false,
                interval_days: 7,
            },
            interval: std::time::Duration::from_secs(7 * 24 * 60 * 60),
            next_run: std::time::Instant::now(),
            job: Some(SubscriptionRefreshJob {
                receiver,
                worker: std::thread::spawn(|| {}),
            }),
            last_report: None,
            last_error: None,
            pending_reload: false,
            next_reload: None,
            last_reload_error: None,
        });
    }

    #[test]
    fn manual_refresh_reports_when_subscriptions_are_unavailable() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('u')).unwrap();
        assert_eq!(
            app.status,
            "Subscription refresh is disabled or .suburl was not found"
        );
    }

    #[test]
    fn changed_node_history_pauses_tui_persistence_and_clears_old_projection() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        let mut app = test_app();
        app.benchmark_workflow.replace_store(Some(store));
        app.usability_probe_projection_cache.insert(
            (
                crate::automatic_selection::NodeViewId::new("criterion").unwrap(),
                "select".to_string(),
            ),
            StoredUsabilityProbeRun {
                run_id: 1,
                completed_at_ms: 1,
                expires_at_ms: None,
                summary: None,
                results: Vec::new(),
                latest_attempt: None,
            },
        );
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                "node-old".to_string(),
                vec![
                    ProbeOutcome::Reachable { delay_ms: 40 },
                    ProbeOutcome::Reachable { delay_ms: 41 },
                    ProbeOutcome::Reachable { delay_ms: 42 },
                ],
            ),
        );
        let cancellation = app
            .benchmark_workflow
            .add_pending_job_for_test("select", "node-old");
        app.node_quality_detail = Some(NodeQualityDetailState {
            selector: "select".to_string(),
            node: "node-old".to_string(),
            last_refresh: std::time::Instant::now(),
            reachability_assessment: None,
            quick_history: Default::default(),
            sustained_quality: None,
            latency_history: Vec::new(),
            throughput_history: Vec::new(),
            auto_selection_detail: None,
            usability_details: Vec::new(),
            evidence_scroll: 0,
        });
        install_completed_refresh(&mut app, &path, true, true);

        app.poll_subscription_refresh_updates_with_reload(|_| anyhow::bail!("restart unavailable"))
            .expect("apply completed changed refresh");

        assert!(!app.benchmark_workflow.quality_persistence_enabled());
        assert!(cancellation.load(Ordering::Relaxed));
        assert!(app.benchmark_workflow.active_nodes("select").is_none());
        assert!(
            app.benchmark_workflow
                .reachability_assessment("select", "node-old")
                .is_none()
        );
        assert!(app.node_quality_detail.is_none());
        assert!(app.usability_probe_projection_cache.is_empty());
        assert_eq!(
            app.benchmark_workflow
                .persist_reachability_for_test("node-old")
                .expect("old result is ignored"),
            None
        );
        assert_eq!(
            app.benchmark_workflow
                .persist_reachability_for_test("node-new")
                .expect("new result is ignored until runtime reload"),
            None
        );
        assert!(app.status.contains("sing-box reload"));
        assert!(app.status.contains("restart unavailable"));
        assert!(app.subscription_refresh.as_ref().unwrap().pending_reload);
        let next_reload = app.subscription_refresh.as_ref().unwrap().next_reload;
        app.try_pending_subscription_reload_with(
            |_| panic!("reload must wait for retry deadline"),
            |_| panic!("refresh must wait for retry deadline"),
        );
        assert_eq!(
            app.subscription_refresh.as_ref().unwrap().next_reload,
            next_reload
        );

        app.subscription_refresh.as_mut().unwrap().next_reload =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        app.try_pending_subscription_reload_with(|_| Ok(()), |_| Ok(()));
        assert!(!app.subscription_refresh.as_ref().unwrap().pending_reload);
        assert!(app.status.contains("sing-box restarted"));
        assert!(
            BenchmarkStore::open(&path)
                .expect("reopen node-quality store")
                .latest_reachability_assessments()
                .expect("read persisted assessments")
                .is_empty()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unchanged_node_history_keeps_tui_persistence_enabled() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [
                    {"type":"selector", "tag":"select", "outbounds":["node-a"]},
                    {"type":"direct", "tag":"node-a"}
                ]
            }))
            .expect("bind unchanged test identities");
        let mut app = test_app();
        app.benchmark_workflow.replace_store(Some(store));
        install_completed_refresh(&mut app, &path, false, false);

        app.poll_subscription_refresh_updates_with_reload(|_| {
            panic!("unchanged config must not restart sing-box")
        })
        .expect("apply completed unchanged refresh");

        assert!(app.benchmark_workflow.quality_persistence_enabled());
        assert_eq!(
            app.benchmark_workflow
                .persist_reachability_for_test("node-a")
                .expect("persist unchanged-generation result"),
            Some(true)
        );
        assert_eq!(
            BenchmarkStore::open(&path)
                .expect("reopen node-quality store")
                .latest_reachability_assessments()
                .expect("read persisted assessments")
                .len(),
            1
        );
        assert!(!app.status.contains("persistence paused"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn config_only_refresh_restarts_managed_runtime() {
        let path = test_db_path();
        let mut app = test_app();
        install_completed_refresh(&mut app, &path, true, false);

        let mut restarted = false;
        app.poll_subscription_refresh_updates_with_reload(|_| {
            restarted = true;
            anyhow::bail!("restart deferred")
        })
        .expect("config-only refresh remains recoverable");

        assert!(restarted);
        assert!(app.subscription_refresh.as_ref().unwrap().pending_reload);
        assert!(app.status.contains("restart deferred"));
    }
}
