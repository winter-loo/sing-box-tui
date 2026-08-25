use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::view::{format_duration_badge, subscription_report_badge, truncate_for_width};
use super::{App, SUBSCRIPTION_REFRESH_RETRY_INTERVAL, TuiSubscriptionRefreshOptions};
use crate::subscriptions::{
    SubscriptionRefreshOutput, SubscriptionRefreshRequest, refresh_subscriptions,
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
    pub(super) fn from_options(options: TuiSubscriptionRefreshOptions) -> Result<Option<Self>> {
        if options.disabled || !options.input.exists() {
            return Ok(None);
        }
        if options.interval_days == 0 {
            bail!("--subscription-interval-days must be greater than 0");
        }
        let interval = Duration::from_secs(options.interval_days.saturating_mul(24 * 60 * 60));
        Ok(Some(Self {
            request: SubscriptionRefreshRequest {
                input: options.input,
                cache_path: options.cache_path,
                config_path: options.config_path.clone(),
                merged_path: options.config_path,
                replace_nodes: false,
                include_geosite_rules: options.include_geosite_rules,
                include_tun_mode: options.include_tun_mode,
                force: options.force,
                interval_days: options.interval_days,
            },
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
            return format!(
                "subscriptions: {}  next in {}  reload sing-box to apply",
                subscription_report_badge(report),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
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
                state.schedule_after(state.interval);
                state.last_error = None;
                state.last_report = Some(report.clone());
                self.set_status_only(format!(
                    "Subscription refresh updated config: {}; reload/restart sing-box to apply",
                    subscription_report_badge(&report)
                ));
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

    use super::super::test_support::test_app;

    #[test]
    fn manual_refresh_reports_when_subscriptions_are_unavailable() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('u')).unwrap();
        assert_eq!(
            app.status,
            "Subscription refresh is disabled or .suburl was not found"
        );
    }
}
