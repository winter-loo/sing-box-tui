use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::App;
use crate::auto_pick::AutoPickDecision;
use crate::benchmark_workflow::{
    BenchmarkCompletion, BenchmarkStart, BenchmarkUpdate, SustainedKind,
    assessment_is_quick_eligible,
};
use crate::controller::{BenchmarkRequest, BenchmarkSummary, ProxyGroup};
use crate::defaults::REFRESH_DEBOUNCE;
use crate::sustained_quality::SustainedProbeRequest;

impl App {
    fn start_sustained_nodes(
        &mut self,
        group: String,
        nodes: Vec<String>,
        kind: SustainedKind,
    ) -> Result<BenchmarkStart> {
        let Some((config_path, sing_box_executable)) = self.sustained_runtime_environment.clone()
        else {
            return Ok(BenchmarkStart::NoCandidates);
        };
        self.benchmark_workflow.start_sustained(
            SustainedProbeRequest {
                selector: group,
                nodes,
                target_url: self.sustained_target_url.clone(),
                config_path,
                sing_box_executable,
            },
            kind,
        )
    }

    pub(super) fn start_group_benchmark(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency tests are available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        let mut candidate_names = self.benchmark_candidates_for_group(&group);
        if let Some(current) = group
            .current
            .as_ref()
            .filter(|current| group.members.contains(*current))
            && !candidate_names.contains(current)
        {
            candidate_names.insert(0, current.clone());
        }
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
        let Some(member) = self.selected_member_name() else {
            self.set_status_only("No node is available in the active node view");
            return Ok(());
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
        let _quality_lease = self.benchmark_workflow.acquire_quality_read_lease()?;
        let _quality_generation = _quality_lease.generation();
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
                self.sync_selection_to_displayed_members();
                self.status = format!("Testing latency for {group}... best so far: {best_label}");
            }
            BenchmarkUpdate::SustainedProgress { group, result } => {
                self.sync_selection_to_displayed_members();
                self.status = format!(
                    "Sustained quality for {group} / {}: {}",
                    result.name,
                    result.compact_evidence()
                );
            }
            BenchmarkUpdate::Disconnected { group } => {
                self.set_status_only(format!("Latency test worker for {group} disconnected"));
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
                group,
                assessed,
                assessments,
                quality_current,
            }) => {
                if !quality_current {
                    self.set_status_only(format!(
                        "Reachability results for {group} were discarded after the managed runtime changed; rerun T"
                    ));
                    return Ok(());
                }
                let status = format!("Reachability assessed {assessed} node(s) in {group}");
                let Some(group_snapshot) = self.group_by_name(&group).cloned() else {
                    self.set_status_only(status);
                    return Ok(());
                };
                let nodes = self.benchmark_workflow.automatic_sustained_candidates(
                    &group,
                    group_snapshot.current.as_deref(),
                    &group_snapshot.members,
                    &assessments,
                );
                // The completion lease cannot span the UI event queue. Revalidate in
                // `start_sustained_nodes`, but treat a concurrent reload as a retriable status
                // instead of terminating the TUI loop.
                match self.start_sustained_nodes(
                    group.clone(),
                    nodes.clone(),
                    SustainedKind::Automatic,
                ) {
                    Ok(BenchmarkStart::Started) => self.set_status_only(format!(
                        "{status}; sustained probing {} node(s) in background",
                        nodes.len()
                    )),
                    Ok(_) => self.set_status_only(status),
                    Err(error) => self.set_status_only(format!(
                        "{status}; sustained probing deferred after runtime change: {error}"
                    )),
                }
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect { group, summary }) => {
                self.finish_auto_select_benchmark(&group, &summary)?;
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::SingleNode {
                group,
                node,
                assessment,
                quality_current,
            }) => {
                if !quality_current {
                    self.set_status_only(format!(
                        "Reachability result for {group} / {node} was discarded after the managed runtime changed; rerun t"
                    ));
                    return Ok(());
                }
                let status = match &assessment {
                    Some(assessment) => format!(
                        "Reachability assessed {group} / {node}: {}",
                        assessment.compact_evidence()
                    ),
                    None => format!("Reachability assessment incomplete for {group} / {node}"),
                };
                if assessment
                    .as_ref()
                    .is_some_and(assessment_is_quick_eligible)
                {
                    match self.start_sustained_nodes(
                        group.clone(),
                        vec![node.clone()],
                        SustainedKind::SingleNode,
                    ) {
                        Ok(BenchmarkStart::Started) => {
                            self.set_status_only(format!("{status}; sustained transfer started"))
                        }
                        Ok(BenchmarkStart::AlreadyRunning) => self
                            .set_status_only(format!("{status}; waiting for sustained transfer")),
                        Ok(_) => self.set_status_only(status),
                        Err(error) => self.set_status_only(format!(
                            "{status}; sustained transfer deferred after runtime change: {error}"
                        )),
                    }
                } else {
                    self.set_status_only(status);
                }
            }
            BenchmarkUpdate::Finished(BenchmarkCompletion::Sustained {
                group,
                kind,
                completed,
                attempted,
                infrastructure_failures,
                cancelled,
            }) => {
                let label = match kind {
                    SustainedKind::Automatic => "Automatic sustained probing",
                    SustainedKind::SingleNode => "Complete node assessment",
                };
                if infrastructure_failures > 0 || cancelled > 0 {
                    self.set_status_only(format!(
                        "{label} incomplete for {group}: {completed}/{attempted} attributable completed; {infrastructure_failures} infrastructure, {cancelled} cancelled"
                    ));
                } else {
                    self.set_status_only(format!(
                        "{label} finished for {group}: {completed}/{attempted} completed"
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tui_benchmark_workflow_tests.rs"]
mod tests;
