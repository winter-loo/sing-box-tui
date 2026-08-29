use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{App, current_unix_timestamp};
use crate::auto_pick::{
    AutoPickConfig, BACKGROUND_TASK_KIND, BackgroundLatencySnapshot, BackgroundLaunchSpec,
    BackgroundPollEvent, BackgroundStatusSnapshot, BackgroundWorkerEnsure, HeadlessWorkerCommand,
    HeadlessWorkerControl, HeadlessWorkerMetadata,
};

fn background_status_should_publish(status: &str) -> bool {
    status.starts_with("Auto-pick") || status.starts_with("Testing latency")
}

fn background_status_requires_selector_refresh(status: &str) -> bool {
    status.starts_with("Auto-pick switched") || status.starts_with("Auto-pick selected")
}

impl App {
    pub(super) fn apply_background_auto_pick_config(&mut self, config: AutoPickConfig) {
        let before = self.auto_pick_runtime_signature();
        self.benchmark_filter = config.filter;
        self.auto_select_enabled = config.enabled;
        self.auto_select_selector = config.selector;
        if !config.benchmark_url.trim().is_empty() {
            self.benchmark_url = config.benchmark_url;
        }
        if config.timeout_ms > 0 {
            self.benchmark_timeout_ms = config.timeout_ms;
        }
        if config.request_timeout > 0.0 {
            self.benchmark_request_timeout = config.request_timeout;
        }
        if config.max_concurrency > 0 {
            self.benchmark_max_concurrency = config.max_concurrency;
        }
        if config.threshold_ms > 0 {
            self.auto_select_threshold_ms = config.threshold_ms;
        }
        if config.interval_secs > 0 {
            self.auto_select_interval = Duration::from_secs(config.interval_secs);
        }
        if before != self.auto_pick_runtime_signature() {
            self.last_auto_select_benchmark = None;
        }
    }

    fn auto_pick_runtime_signature(
        &self,
    ) -> (
        bool,
        Option<String>,
        String,
        String,
        u64,
        u64,
        u64,
        u64,
        usize,
    ) {
        (
            self.auto_select_enabled,
            self.auto_select_selector.clone(),
            self.benchmark_filter.clone(),
            self.benchmark_url.clone(),
            self.benchmark_timeout_ms,
            self.benchmark_request_timeout.to_bits(),
            self.auto_select_threshold_ms,
            self.auto_select_interval.as_secs(),
            self.benchmark_max_concurrency,
        )
    }

    pub(super) fn auto_pick_config(&self) -> AutoPickConfig {
        AutoPickConfig {
            enabled: self.auto_select_enabled,
            selector: self.auto_select_selector.clone(),
            filter: self.benchmark_filter.clone(),
            benchmark_url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            threshold_ms: self.auto_select_threshold_ms,
            interval_secs: self.auto_select_interval.as_secs(),
        }
    }

    pub(super) fn background_launch_spec(&self) -> Result<BackgroundLaunchSpec> {
        let runtime_receipt = self
            .benchmark_workflow
            .runtime_receipt()
            .cloned()
            .context("background auto-pick requires a confirmed managed runtime receipt")?;
        Ok(BackgroundLaunchSpec::new(
            self.client.base_url.clone(),
            self.system_proxy_config_path.clone(),
            self.benchmark_max_concurrency,
            runtime_receipt,
        ))
    }

    pub(super) fn poll_background_auto_pick_status(&mut self) -> Result<()> {
        if !self.background_worker_management_enabled() {
            return Ok(());
        }
        let config = self.auto_pick_config();
        let launch = self.background_launch_spec()?;
        let Some(event) =
            self.background_auto_pick
                .poll(self.auto_select_enabled, &config, &launch)?
        else {
            return Ok(());
        };
        match event {
            BackgroundPollEvent::Update(update) => {
                self.apply_background_latency_snapshot(update.latency.as_ref());
                if let Some(status) = update.status {
                    if background_status_requires_selector_refresh(&status) {
                        self.refresh()?;
                    }
                    self.set_status_only(format!("Auto-pick worker: {status}"));
                }
            }
            BackgroundPollEvent::Retry(error) => self.set_status_only(format!(
                "Auto-pick worker TCP error; process is still alive, retrying: {error}"
            )),
            BackgroundPollEvent::Exited(error) => {
                self.set_status_only(format!("Auto-pick worker exited after TCP error: {error}"))
            }
            BackgroundPollEvent::Restarted(worker) => self.set_status_only(format!(
                "Auto-pick background worker {} pid {} after previous worker exited",
                worker.label(),
                worker.pid()
            )),
            BackgroundPollEvent::Ensured(worker) => self.set_status_only(format!(
                "Auto-pick background worker {} pid {}",
                worker.label(),
                worker.pid()
            )),
        }
        Ok(())
    }

    pub(super) fn apply_background_latency_snapshot(
        &mut self,
        latency: Option<&BackgroundLatencySnapshot>,
    ) {
        let Some(latency) = latency else {
            return;
        };
        self.benchmark_workflow
            .apply_background_snapshot(latency, &self.benchmark_filter);
    }

    pub(super) fn ensure_auto_pick_background_worker_if_enabled(&mut self) -> Result<()> {
        if !self.auto_select_enabled || !self.background_worker_management_enabled() {
            return Ok(());
        }
        let worker = self.ensure_auto_pick_background_worker()?;
        self.set_status_only(format!(
            "Auto-pick background worker {} pid {}",
            worker.label(),
            worker.pid()
        ));
        Ok(())
    }

    pub(super) fn ensure_auto_pick_background_worker_after_state_change(&mut self) -> Result<()> {
        if self.auto_select_enabled && self.background_worker_management_enabled() {
            self.ensure_auto_pick_background_worker()?;
        }
        Ok(())
    }

    pub(super) fn background_worker_management_enabled(&self) -> bool {
        self.state_store.is_some() && !cfg!(test)
    }

    pub(super) fn ensure_auto_pick_background_worker(&mut self) -> Result<BackgroundWorkerEnsure> {
        let config = self.auto_pick_config();
        let launch = self.background_launch_spec()?;
        self.background_auto_pick.ensure(&config, &launch)
    }

    pub(super) fn stop_live_background_auto_pick_task(&mut self) -> Result<()> {
        self.background_auto_pick.stop()
    }

    pub(super) fn background_status_snapshot(
        &self,
        worker_status: String,
        generation: u64,
    ) -> BackgroundStatusSnapshot {
        let runtime_receipt = self.benchmark_workflow.runtime_receipt();
        BackgroundStatusSnapshot {
            kind: BACKGROUND_TASK_KIND.to_string(),
            pid: std::process::id(),
            controller: self.client.base_url.clone(),
            config_path: self.system_proxy_config_path.clone(),
            quality_generation: runtime_receipt
                .map(|receipt| receipt.quality_generation())
                .unwrap_or(u64::MAX),
            managed_pid: runtime_receipt.and_then(|receipt| receipt.managed_pid()),
            max_concurrency: self.benchmark_max_concurrency,
            started_at_unix: self.background_started_at_unix,
            status_generation: generation,
            worker_status,
            updated_at_unix: current_unix_timestamp(),
            auto_pick_enabled: self.auto_select_enabled,
            auto_pick_selector: self.auto_select_selector.clone(),
            filter: self.benchmark_filter.clone(),
            latency: self.background_latency_snapshot(),
        }
    }

    pub(super) fn background_latency_snapshot(&self) -> Option<BackgroundLatencySnapshot> {
        let group = self.auto_select_group()?;
        self.benchmark_workflow.background_snapshot(&group.name)
    }

    pub(super) fn run_headless_auto_pick_loop(&mut self) -> Result<()> {
        let control = HeadlessWorkerControl::start(HeadlessWorkerMetadata::new(
            self.client.base_url.clone(),
            self.system_proxy_config_path.clone(),
            self.benchmark_max_concurrency,
            self.background_started_at_unix,
        ))?;
        self.auto_select_enabled = false;
        let mut last_published_status = String::new();
        let mut status_generation = 0;
        loop {
            while let Some(request) = control.try_request() {
                match request.command.clone() {
                    HeadlessWorkerCommand::Status => request.respond(
                        self.background_status_snapshot(self.status.clone(), status_generation),
                    ),
                    HeadlessWorkerCommand::ApplyConfig(config) => {
                        self.apply_background_auto_pick_config(config);
                        last_published_status.clear();
                        status_generation = status_generation.saturating_add(1);
                        request.respond(self.background_status_snapshot(
                            "configuration applied".to_string(),
                            status_generation,
                        ));
                    }
                    HeadlessWorkerCommand::Stop => {
                        status_generation = status_generation.saturating_add(1);
                        request.respond(
                            self.background_status_snapshot(
                                "stopping".to_string(),
                                status_generation,
                            ),
                        );
                        control.unregister();
                        return Ok(());
                    }
                }
            }

            if self.auto_select_enabled {
                self.poll_benchmark_updates()?;
                self.maybe_start_auto_select_benchmark()?;
                if self.status != last_published_status
                    && background_status_should_publish(&self.status)
                {
                    status_generation = status_generation.saturating_add(1);
                    last_published_status = self.status.clone();
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
#[path = "tui_auto_pick_worker_tests.rs"]
mod tests;
