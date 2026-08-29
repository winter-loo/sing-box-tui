use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::App;
use crate::auto_pick::AutoPickDecision;
use crate::benchmark_workflow::{BenchmarkCompletion, BenchmarkStart, BenchmarkUpdate};
use crate::controller::{BenchmarkRequest, BenchmarkSummary, ProxyGroup};
use crate::defaults::REFRESH_DEBOUNCE;

impl App {
    pub(super) fn start_group_benchmark(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency tests are available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        let candidate_names = self.benchmark_candidates_for_group(&group);
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: Some(candidate_names.clone()),
        };
        match self.benchmark_workflow.start_group(request) {
            BenchmarkStart::Started => self.set_status_only(format!(
                "Testing latency for {} with filter '{}' in background (max {} concurrent)...",
                group.name, self.benchmark_filter, self.benchmark_max_concurrency
            )),
            BenchmarkStart::AlreadyRunning => {
                self.set_status_only(format!("Latency test already running for {}", group.name))
            }
            BenchmarkStart::NoCandidates => self.set_status_only(format!(
                "No nodes in {} matched filter '{}'",
                group.name, self.benchmark_filter
            )),
            BenchmarkStart::Debounced => {}
            BenchmarkStart::CancellationRequested => self.set_status_only(format!(
                "Cancelling reachability assessment for {}",
                group.name
            )),
        }
        Ok(())
    }

    pub(super) fn start_member_benchmark(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency tests are available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        let displayed_members = self.displayed_members();
        let Some(member) = self
            .displayed_member_index()
            .and_then(|index| displayed_members.get(index))
            .cloned()
        else {
            bail!("no proxy available in selected group");
        };
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: 1,
            nodes: Some(vec![member.clone()]),
        };
        match self
            .benchmark_workflow
            .start_single_node(request, member.clone())
        {
            BenchmarkStart::Started => self.set_status_only(format!(
                "Testing latency for {} / {} in background...",
                group.name, member
            )),
            BenchmarkStart::AlreadyRunning => self.set_status_only(format!(
                "Latency test already running for {} / {}",
                group.name, member
            )),
            BenchmarkStart::Debounced => self.set_status_only(format!(
                "Ignoring repeated retest for {} / {} (debounced)",
                group.name, member
            )),
            BenchmarkStart::NoCandidates => {}
            BenchmarkStart::CancellationRequested => self.set_status_only(format!(
                "Cancelling reachability assessment for {} / {}",
                group.name, member
            )),
        }
        Ok(())
    }

    pub(super) fn toggle_latency_sort_mode(&mut self) {
        let status = if self.benchmark_workflow.toggle_latency_order() {
            "Sort order: LATENCY ORDER (sort successful nodes by delay, retain all members)"
                .to_string()
        } else {
            "Sort order: SELECTOR ORDER (complete original selector order)".to_string()
        };
        self.set_status_only(status);
    }

    pub(super) fn toggle_auto_select(&mut self) -> Result<()> {
        if self.auto_select_enabled {
            self.auto_select_enabled = false;
            self.auto_select_selector = None;
            self.save_runtime_state()?;
            if self.background_worker_management_enabled() {
                self.stop_live_background_auto_pick_task()?;
            }
            self.set_status_only("Auto-pick disabled; background worker stopped");
            return Ok(());
        }

        let Some(group_name) = self.selected_group().map(|group| group.name.clone()) else {
            self.set_status_only("No selector group available for auto-pick");
            return Ok(());
        };
        self.auto_select_enabled = true;
        self.auto_select_selector = Some(group_name.clone());
        self.last_auto_select_benchmark = None;
        self.save_runtime_state()?;
        if self.background_worker_management_enabled() {
            let worker = self.ensure_auto_pick_background_worker()?;
            self.set_status_only(format!(
                "Auto-pick enabled for {} via background worker pid {} ({}, {}ms threshold, every {}s)",
                group_name,
                worker.pid(),
                self.benchmark_scope_label(),
                self.auto_select_threshold_ms,
                self.auto_select_interval.as_secs()
            ));
        } else {
            self.set_status_only(format!(
                "Auto-pick enabled for {} ({}, {}ms threshold, every {}s)",
                group_name,
                self.benchmark_scope_label(),
                self.auto_select_threshold_ms,
                self.auto_select_interval.as_secs()
            ));
        }
        Ok(())
    }

    pub(super) fn auto_select_benchmark_due(&self, now: Instant) -> bool {
        self.auto_pick_config()
            .benchmark_due(self.last_auto_select_benchmark, now)
    }

    pub(super) fn maybe_start_auto_select_benchmark(&mut self) -> Result<()> {
        let now = Instant::now();
        if !self.auto_select_benchmark_due(now) {
            return Ok(());
        }
        let Some(group) = self.auto_select_group().cloned() else {
            return Ok(());
        };
        let candidate_names = self.benchmark_candidates_for_group(&group);
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: Some(candidate_names.clone()),
        };
        match self.benchmark_workflow.start_auto_select(request) {
            BenchmarkStart::Started => {
                self.last_auto_select_benchmark = Some(now);
                self.set_status_only(format!(
                    "Auto-pick testing latency for {} ({})...",
                    group.name,
                    self.benchmark_scope_label()
                ));
            }
            BenchmarkStart::NoCandidates => {
                self.last_auto_select_benchmark = Some(now);
                self.set_status_only(format!(
                    "Auto-pick found no nodes in {} for {}",
                    group.name,
                    self.benchmark_scope_label()
                ));
            }
            BenchmarkStart::AlreadyRunning
            | BenchmarkStart::Debounced
            | BenchmarkStart::CancellationRequested => {}
        }
        Ok(())
    }

    pub(super) fn benchmark_scope_label(&self) -> String {
        self.auto_pick_config().scope_label()
    }

    pub(super) fn auto_select_group(&self) -> Option<&ProxyGroup> {
        self.auto_select_selector
            .as_deref()
            .and_then(|selector| self.group_by_name(selector))
            .or_else(|| self.selected_group())
    }

    pub(super) fn auto_select_switch_plan(
        &self,
        group: &ProxyGroup,
        summary: &BenchmarkSummary,
    ) -> AutoPickDecision {
        self.auto_pick_config().switch_decision(
            group,
            summary,
            self.implicit_root_parent_switch_for_group(&group.name),
        )
    }

    pub(super) fn finish_auto_select_benchmark(
        &mut self,
        group_name: &str,
        summary: &BenchmarkSummary,
    ) -> Result<()> {
        let Some(group) = self
            .groups
            .iter()
            .find(|group| group.name == group_name)
            .cloned()
        else {
            self.set_status_only(format!(
                "Auto-pick finished for missing group {}",
                group_name
            ));
            return Ok(());
        };

        let plan = self.auto_select_switch_plan(&group, summary);
        if plan.target_node.is_none() && plan.parent_switch.is_none() {
            let current = group.current.as_deref().unwrap_or("unset");
            self.set_status_only(format!(
                "Auto-pick kept {} on {} (threshold {}ms)",
                group_name, current, self.auto_select_threshold_ms
            ));
            return Ok(());
        }

        if let Some(target) = &plan.target_node {
            self.client
                .switch_proxy(group_name, target)
                .with_context(|| {
                    format!("auto-pick failed to switch {} to {}", group_name, target)
                })?;
        }
        if let Some((parent, route_group)) = &plan.parent_switch {
            self.client
                .switch_proxy(parent, route_group)
                .with_context(|| {
                    format!("auto-pick failed to switch {} to {}", parent, route_group)
                })?;
        }
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        match (&plan.target_node, &plan.parent_switch) {
            (Some(target), Some((_, route_group))) => self.set_status_only(format!(
                "Auto-pick switched {} to {} and selected {}",
                group_name, target, route_group
            )),
            (Some(target), None) => {
                self.set_status_only(format!("Auto-pick switched {} to {}", group_name, target))
            }
            (None, Some((_, route_group))) => {
                let current = group.current.as_deref().unwrap_or("unset");
                self.set_status_only(format!(
                    "Auto-pick selected {}; kept {} on {} (threshold {}ms)",
                    route_group, group_name, current, self.auto_select_threshold_ms
                ));
            }
            (None, None) => {}
        }
        Ok(())
    }

    pub(super) fn poll_benchmark_updates(&mut self) -> Result<()> {
        for update in self.benchmark_workflow.poll() {
            self.apply_benchmark_update(update)?;
        }
        Ok(())
    }

    pub(super) fn apply_benchmark_update(&mut self, update: BenchmarkUpdate) -> Result<()> {
        match update {
            BenchmarkUpdate::Progress { group, best_label } => {
                self.status = format!("Testing latency for {group}... best so far: {best_label}");
            }
            BenchmarkUpdate::Disconnected { group } => {
                self.set_status_only(format!("Latency test worker for {group} disconnected"));
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::Group { group, assessed }) => {
                self.set_status_only(format!(
                    "Reachability assessed {assessed} node(s) in {group}"
                ));
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect { group, summary }) => {
                self.finish_auto_select_benchmark(&group, &summary)?;
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::SingleNode {
                group,
                node,
                assessment,
            }) => {
                let status = match assessment {
                    Some(assessment) => format!(
                        "Reachability assessed {group} / {node}: {}",
                        assessment.compact_evidence()
                    ),
                    None => format!("Reachability assessment incomplete for {group} / {node}"),
                };
                self.set_status_only(status);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tui_benchmark_workflow_tests.rs"]
mod tests;
