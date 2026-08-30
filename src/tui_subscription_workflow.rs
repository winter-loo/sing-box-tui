use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::view::{format_duration_badge, subscription_report_badge, truncate_for_width};
use super::{App, SUBSCRIPTION_REFRESH_RETRY_INTERVAL, TuiSubscriptionRefreshOptions};
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
        if let Some(error) = &state.last_error {
            return format!(
                "subscriptions: error: {}  retry in {}",
                truncate_for_width(error, 72),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
            );
        }
        if let Some(report) = &state.last_report {
            let reload_note = if report.node_history_changed {
                "  node-quality persistence paused until a managed sing-box reload is confirmed"
            } else if report.config_updated {
                "  reload sing-box to apply"
            } else {
                ""
            };
            return format!(
                "subscriptions: {}  next in {}{}",
                subscription_report_badge(report),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now())),
                reload_note
            );
        }
        format!(
            "subscriptions: pending first refresh from {}",
            state.request.input.display()
        )
    }

    pub(super) fn maybe_start_subscription_refresh(&mut self) {
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
                    self.latency_chart = None;
                }
                state.schedule_after(state.interval);
                state.last_error = None;
                state.last_report = Some(report.clone());
                let status = if report.node_history_changed {
                    format!(
                        "Subscription refresh updated config: {}; node-quality persistence paused until a managed sing-box reload is confirmed",
                        subscription_report_badge(&report)
                    )
                } else if report.config_updated {
                    format!(
                        "Subscription refresh updated config: {}; reload/restart sing-box to apply",
                        subscription_report_badge(&report)
                    )
                } else {
                    format!(
                        "Subscription refresh kept the active config unchanged: {}",
                        subscription_report_badge(&report)
                    )
                };
                self.set_status_only(status);
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
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use std::sync::atomic::Ordering;

    use super::super::LatencyChartState;
    use super::{SubscriptionRefreshEvent, SubscriptionRefreshJob, SubscriptionRefreshState};
    use crate::controller::{BenchmarkSummary, NodeReachabilityAssessment, ProbeOutcome};
    use crate::storage::BenchmarkStore;
    use crate::subscriptions::{SubscriptionRefreshOutput, SubscriptionRefreshRequest};

    use super::super::test_support::{test_app, test_db_path};

    fn install_completed_refresh(
        app: &mut super::super::App,
        database_path: &std::path::Path,
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
                    config_updated: true,
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
        app.benchmark_workflow
            .set_summary(BenchmarkSummary::empty("select".to_string()));
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
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-old".to_string(),
            samples: Vec::new(),
            window: std::time::Duration::from_secs(300),
            threshold_ms: 800,
            last_refresh: std::time::Instant::now(),
            reachability_assessment: None,
            sustained_quality: None,
            auto_selection_detail: None,
        });
        install_completed_refresh(&mut app, &path, true);

        app.poll_subscription_refresh_updates()
            .expect("apply completed changed refresh");

        assert!(!app.benchmark_workflow.quality_persistence_enabled());
        assert!(cancellation.load(Ordering::Relaxed));
        assert!(app.benchmark_workflow.active_nodes("select").is_none());
        assert!(app.benchmark_workflow.summary("select").is_none());
        assert!(
            app.benchmark_workflow
                .reachability_assessment("select", "node-old")
                .is_none()
        );
        assert!(app.latency_chart.is_none());
        assert_eq!(
            app.benchmark_workflow
                .persist_benchmark_for_test("node-old")
                .expect("old result is ignored"),
            None
        );
        assert_eq!(
            app.benchmark_workflow
                .persist_benchmark_for_test("node-new")
                .expect("new result is ignored until runtime reload"),
            None
        );
        assert!(app.status.contains("node-quality persistence paused"));
        assert!(
            BenchmarkStore::open(&path)
                .expect("reopen benchmark store")
                .recent_benchmarks(10)
                .expect("read persisted rows")
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
        install_completed_refresh(&mut app, &path, false);

        app.poll_subscription_refresh_updates()
            .expect("apply completed unchanged refresh");

        assert!(app.benchmark_workflow.quality_persistence_enabled());
        assert_eq!(
            app.benchmark_workflow
                .persist_benchmark_for_test("node-a")
                .expect("persist unchanged-generation result"),
            Some(true)
        );
        assert_eq!(
            BenchmarkStore::open(&path)
                .expect("reopen benchmark store")
                .recent_benchmarks(10)
                .expect("read persisted rows")
                .len(),
            1
        );
        assert!(!app.status.contains("persistence paused"));
        let _ = std::fs::remove_file(path);
    }
}
